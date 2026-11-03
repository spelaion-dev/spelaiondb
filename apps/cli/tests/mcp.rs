//! `spelunk mcp` end to end: a fake agent speaks JSON-RPC over stdio with the real binary,
//! as Claude Code would.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_spelunk");

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("spelunk-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelunk/connections")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn repo(&self) -> PathBuf {
        self.base.join("repo")
    }

    fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    fn spelunk(&self, args: &[&str]) -> String {
        let out = Command::new(BIN)
            .args(args)
            .env("SPELUNK_USER_DIR", self.user())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "spelunk {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Minimal JSON-RPC client over stdio (one JSON message per line).
struct Agent {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Agent {
    fn start(fx: &Fixture) -> Self {
        let mut child = Command::new(BIN)
            .args(["mcp", "--workspace"])
            .arg(fx.repo())
            .env("SPELUNK_USER_DIR", fx.user())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut agent = Self {
            child,
            stdin,
            stdout,
            next_id: 1,
        };
        let init = agent.request(
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test-agent", "version": "1.0" }
            }),
        );
        assert_eq!(init["serverInfo"]["name"], "spelunkdb", "{init}");
        agent.send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        agent
    }

    fn send(&mut self, message: Value) {
        let stdin = self.stdin.as_mut().unwrap();
        writeln!(stdin, "{message}").unwrap();
        stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let mut line = String::new();
            assert!(
                self.stdout.read_line(&mut line).unwrap() > 0,
                "the server closed stdout"
            );
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|e| panic!("stdout is not JSON-RPC ({e}): {line}"));
            if message["id"] == json!(id) {
                assert!(message.get("error").is_none(), "{method}: {message}");
                return message["result"].clone();
            }
        }
    }

    /// Calls a tool: (text, is_error).
    fn call(&mut self, tool: &str, arguments: Value) -> (String, bool) {
        let result = self.request(
            "tools/call",
            json!({ "name": tool, "arguments": arguments }),
        );
        let text = result["content"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        (text, result["isError"].as_bool().unwrap_or(false))
    }

    fn stop(mut self) {
        drop(self.stdin.take());
        let status = self.child.wait().unwrap();
        assert!(status.success(), "spelunk mcp exited with {status}");
    }
}

fn create_db(path: &Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(
        "create table products (id integer primary key, name text not null, price real);
         insert into products (name, price) values ('lamp', 19.9), ('table', 120), ('chair', 45);",
    )
    .unwrap();
}

#[test]
fn an_agent_can_read_but_never_write() {
    let fx = Fixture::new("mcp");
    create_db(&fx.repo().join("shop.db"));
    std::fs::write(
        fx.repo().join(".spelunk/connections/shop.toml"),
        "schema_version = 1\nname = \"Shop\"\nengine = \"sqlite\"\ndatabase = \"shop.db\"\n\n[mcp]\nexposed = true\nmax_rows = 2\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/hidden.toml"),
        "schema_version = 1\nengine = \"sqlite\"\ndatabase = \"shop.db\"\n",
    )
    .unwrap();

    // Without trusting the repository, the agent does not see its connections.
    let mut agent = Agent::start(&fx);
    let (text, _) = agent.call("list_connections", json!({}));
    assert!(
        text.contains("is not trusted") && !text.contains("- shop"),
        "{text}"
    );
    agent.stop();

    fx.spelunk(&["trust", fx.repo().to_str().unwrap()]);
    let mut agent = Agent::start(&fx);

    // Every tool but execute_write is announced as read-only.
    let tools = agent.request("tools/list", json!({}));
    let tools = tools["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in [
        "list_connections",
        "list_objects",
        "describe_table",
        "search_schema",
        "run_query",
        "explain_query",
        "execute_write",
    ] {
        assert!(names.contains(&expected), "missing {expected}: {names:?}");
    }
    for tool in tools {
        let read_only = tool["name"] != json!("execute_write");
        assert_eq!(
            tool["annotations"]["readOnlyHint"],
            json!(read_only),
            "{tool:?}"
        );
    }

    // Only exposed connections.
    let (text, _) = agent.call("list_connections", json!({}));
    assert!(
        text.contains("- shop — Shop (sqlite). Read-only.") && !text.contains("hidden"),
        "{text}"
    );
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "hidden", "sql": "select 1" }),
    );
    assert!(is_error && text.contains("is not exposed"), "{text}");

    // A read with a row limit (max_rows = 2).
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "shop", "sql": "select name, price from products order by id" }),
    );
    assert!(!is_error, "{text}");
    assert!(
        text.contains("| lamp | 19.9 |") && text.contains("| table | 120 |"),
        "{text}"
    );
    assert!(
        !text.contains("chair") && text.contains("there are more"),
        "{text}"
    );

    // Exploration.
    let (text, _) = agent.call(
        "list_objects",
        json!({ "connection": "shop", "schema": "main" }),
    );
    assert!(text.contains("- products"), "{text}");
    let (text, _) = agent.call(
        "describe_table",
        json!({ "connection": "shop", "schema": "main", "table": "products" }),
    );
    assert!(
        text.contains("- name TEXT NOT NULL") && text.contains("Primary key: (id)"),
        "{text}"
    );
    let (text, _) = agent.call(
        "search_schema",
        json!({ "connection": "shop", "query": "prod" }),
    );
    assert!(text.contains("main.products"), "{text}");
    let (text, is_error) = agent.call(
        "explain_query",
        json!({ "connection": "shop", "sql": "select * from products where id = 1" }),
    );
    assert!(!is_error, "{text}");

    // Writes: rejected before reaching the database.
    for sql in [
        "delete from products",
        "drop table products",
        "update products set price = 0",
        "attach 'x.db' as x",
    ] {
        let (text, is_error) = agent.call("run_query", json!({ "connection": "shop", "sql": sql }));
        assert!(is_error && text.contains("rejected"), "{sql}: {text}");
    }
    let (text, is_error) = agent.call(
        "explain_query",
        json!({ "connection": "shop", "sql": "analyze delete from products" }),
    );
    assert!(is_error, "EXPLAIN ANALYZE should be rejected: {text}");
    // The connection does not allow writes.
    let (text, is_error) = agent.call(
        "execute_write",
        json!({ "connection": "shop", "sql": "delete from products where id = 1" }),
    );
    assert!(is_error && text.contains("read-only for agents"), "{text}");
    agent.stop();

    let conn = rusqlite::Connection::open(fx.repo().join("shop.db")).unwrap();
    let count: i64 = conn
        .query_row("select count(*) from products", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 3, "the database should not change");

    // Everything is recorded in the audit log, with the declared client.
    let audit = fx.spelunk(&["audit", "--limit", "50"]);
    assert!(
        audit.contains("test-agent · shop · REJECTED: delete from products"),
        "{audit}"
    );
    assert!(
        audit.contains("test-agent · shop · executed: select name, price from products"),
        "{audit}"
    );
    assert!(audit.contains("· hidden · REJECTED"), "{audit}");
}

#[test]
fn an_agent_writes_only_where_the_user_allowed_it() {
    let fx = Fixture::new("mcp-write");
    create_db(&fx.repo().join("shop.db"));
    std::fs::write(
        fx.repo().join(".spelunk/connections/shop.toml"),
        "schema_version = 1\nname = \"Shop\"\nengine = \"sqlite\"\ndatabase = \"shop.db\"\n\n[mcp]\nexposed = true\nwrites = \"allow\"\n",
    )
    .unwrap();
    fx.spelunk(&["trust", fx.repo().to_str().unwrap()]);
    let mut agent = Agent::start(&fx);

    let (text, _) = agent.call("list_connections", json!({}));
    assert!(
        text.contains("- shop — Shop (sqlite). Read and write"),
        "{text}"
    );

    let (text, is_error) = agent.call(
        "execute_write",
        json!({ "connection": "shop", "sql": "update products set price = 10 where name = 'lamp'" }),
    );
    assert!(!is_error && text.contains("1 row(s) affected"), "{text}");
    let (text, is_error) = agent.call(
        "execute_write",
        json!({ "connection": "shop", "sql": "insert into products (name, price) values ('desk', 80), ('shelf', 30);" }),
    );
    assert!(!is_error && text.contains("2 row(s) affected"), "{text}");

    // Barriers that no setting lifts, and a failure that changes nothing.
    for (sql, why) in [
        ("drop table products", "DROP"),
        ("delete from products", "DELETE without WHERE"),
        ("select * from products", "run_query"),
        ("attach 'x.db' as x", "other files"),
        ("begin", "transaction"),
        (
            "delete from products where id = 1; drop table products",
            "one statement",
        ),
    ] {
        let (text, is_error) =
            agent.call("execute_write", json!({ "connection": "shop", "sql": sql }));
        assert!(is_error && text.contains(why), "{sql}: {text}");
    }
    let (text, is_error) = agent.call(
        "execute_write",
        json!({ "connection": "shop", "sql": "insert into products (name) values ('ok'), (null)" }),
    );
    assert!(is_error && text.contains("rolled back"), "{text}");

    // Reads still go through the read-only session, and see the writes.
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "shop", "sql": "select count(*) as n from products" }),
    );
    assert!(!is_error && text.contains("| 5 |"), "{text}");
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "shop", "sql": "delete from products" }),
    );
    assert!(is_error && text.contains("rejected"), "{text}");
    agent.stop();

    let conn = rusqlite::Connection::open(fx.repo().join("shop.db")).unwrap();
    let (count, lamp): (i64, f64) = conn
        .query_row(
            "select count(*), (select price from products where name = 'lamp') from products",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((count, lamp), (5, 10.0));

    let audit = fx.spelunk(&["audit", "--limit", "50"]);
    assert!(
        audit.contains("test-agent · shop · executed: update products set price = 10"),
        "{audit}"
    );
    assert!(
        audit.contains("test-agent · shop · REJECTED: drop table products"),
        "{audit}"
    );
    assert!(
        audit.contains(
            "test-agent · shop · failed: insert into products (name) values ('ok'), (null)"
        ),
        "{audit}"
    );
}
