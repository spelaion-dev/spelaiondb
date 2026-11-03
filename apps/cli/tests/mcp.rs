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

/// Runs `script` on a DuckDB file, creating it, with the project's own driver.
fn duckdb(path: &Path, script: &[&str]) -> Vec<String> {
    use futures_util::StreamExt;
    use spelunk_driver_api::{ConnectParams, Driver, ExecEvent, ExecOptions, Value};
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let driver = spelunk_driver_duckdb::DuckdbDriver {
            create_if_missing: true,
        };
        let conn = driver
            .connect(&ConnectParams {
                database: path.display().to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut values = Vec::new();
        for sql in script {
            let mut events = conn.execute(sql, &ExecOptions::default()).await.unwrap();
            let mut logical = None;
            while let Some(event) = events.next().await {
                match event.unwrap() {
                    ExecEvent::Columns(schema) => logical = Some(schema.columns[0].logical),
                    ExecEvent::Batch(batch) if batch.num_rows() > 0 => {
                        let value =
                            Value::from_array(batch.column(0).as_ref(), 0, logical.unwrap())
                                .unwrap();
                        values.push(value.to_string());
                    }
                    _ => {}
                }
            }
        }
        values
    })
}

#[test]
fn duckdb_connections_serve_agents_without_files() {
    let fx = Fixture::new("mcp-duckdb");
    duckdb(
        &fx.repo().join("shop.duckdb"),
        &[
            "create table products (id integer primary key, name varchar not null); \
           insert into products values (1, 'lamp'), (2, 'table'), (3, 'chair')",
        ],
    );
    duckdb(
        &fx.repo().join("archive.duckdb"),
        &["create table products as select 1 as id"],
    );
    std::fs::write(fx.repo().join("secret.csv"), "password\nhunter2\n").unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/shop.toml"),
        "schema_version = 1\nname = \"Shop\"\nengine = \"duckdb\"\ndatabase = \"shop.duckdb\"\n\n[mcp]\nexposed = true\nwrites = \"allow\"\n",
    )
    .unwrap();
    std::fs::write(
        fx.repo().join(".spelunk/connections/archive.toml"),
        "schema_version = 1\nname = \"Archive\"\nengine = \"duckdb\"\ndatabase = \"archive.duckdb\"\n\n[mcp]\nexposed = true\n",
    )
    .unwrap();
    fx.spelunk(&["trust", fx.repo().to_str().unwrap()]);
    let mut agent = Agent::start(&fx);

    let (text, _) = agent.call("list_connections", json!({}));
    assert!(
        text.contains("- shop — Shop (duckdb). Read and write")
            && text.contains("- archive — Archive (duckdb). Read-only."),
        "{text}"
    );
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "shop", "sql": "select name from products order by id" }),
    );
    assert!(!is_error && text.contains("| lamp |"), "{text}");
    let (text, is_error) = agent.call(
        "execute_write",
        json!({ "connection": "shop", "sql": "insert into products values (4, 'desk')" }),
    );
    assert!(!is_error && text.contains("1 row(s) affected"), "{text}");
    // Reads share the writable file, and see the write.
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "shop", "sql": "select count(*) as n from products" }),
    );
    assert!(!is_error && text.contains("| 4 |"), "{text}");
    let (text, is_error) = agent.call(
        "explain_query",
        json!({ "connection": "shop", "sql": "select * from products" }),
    );
    assert!(!is_error, "{text}");

    // No files, by name or as a table, not even through a write.
    let csv = fx
        .repo()
        .join("secret.csv")
        .display()
        .to_string()
        .replace('\\', "/");
    for (tool, sql) in [
        ("run_query", format!("select * from read_csv('{csv}')")),
        ("run_query", format!("select * from '{csv}'")),
        (
            "execute_write",
            format!("insert into products select 9, password from '{csv}'"),
        ),
        ("run_query", "select nextval('x')".into()),
    ] {
        let (text, is_error) = agent.call(tool, json!({ "connection": "shop", "sql": sql }));
        assert!(is_error && !text.contains("hunter2"), "{sql}: {text}");
    }

    // The other connection does not let agents write.
    let (text, is_error) = agent.call(
        "execute_write",
        json!({ "connection": "archive", "sql": "insert into products values (2)" }),
    );
    assert!(is_error && text.contains("read-only for agents"), "{text}");
    let (text, is_error) = agent.call(
        "run_query",
        json!({ "connection": "archive", "sql": "select count(*) as n from products" }),
    );
    assert!(!is_error && text.contains("| 1 |"), "{text}");
    agent.stop();

    assert_eq!(
        duckdb(
            &fx.repo().join("shop.duckdb"),
            &["select string_agg(name, ',' order by id) from products"]
        ),
        ["lamp,table,chair,desk"]
    );
}

#[test]
fn a_connection_changed_after_trusting_is_refused_until_reviewed() {
    let fx = Fixture::new("mcp-pinned");
    create_db(&fx.repo().join("shop.db"));
    create_db(&fx.repo().join("other.db"));
    let file = fx.repo().join(".spelunk/connections/shop.toml");
    let content = "schema_version = 1\nname = \"Shop\"\nengine = \"sqlite\"\ndatabase = \"shop.db\"\n\n[mcp]\nexposed = true\n";
    std::fs::write(&file, content).unwrap();
    fx.spelunk(&["trust", fx.repo().to_str().unwrap()]);
    let query = json!({ "connection": "shop", "sql": "select count(*) as n from products" });

    let mut agent = Agent::start(&fx);
    let (text, is_error) = agent.call("run_query", query.clone());
    assert!(!is_error, "{text}");
    agent.stop();

    // A pull points the connection somewhere else: the agent cannot use it.
    std::fs::write(&file, content.replace("shop.db", "other.db")).unwrap();
    let mut agent = Agent::start(&fx);
    let (text, is_error) = agent.call("run_query", query.clone());
    assert!(
        is_error
            && text.contains("changed in the repository since you trusted it")
            && text.contains("database shop.db → other.db"),
        "{text}"
    );
    agent.stop();

    // `spelunk trust` says what it accepts; then it works again.
    let out = fx.spelunk(&["trust", fx.repo().to_str().unwrap()]);
    assert!(
        out.contains("Accepted: the connection shop changed")
            && out.contains("database shop.db → other.db"),
        "{out}"
    );
    let mut agent = Agent::start(&fx);
    let (text, is_error) = agent.call("run_query", query);
    assert!(!is_error && text.contains("| 3 |"), "{text}");
    agent.stop();
}

#[test]
fn a_connection_missing_personal_variables_says_which_ones() {
    let fx = Fixture::new("mcp-vars");
    create_db(&fx.repo().join("shop.db"));
    std::fs::write(
        fx.repo().join(".spelunk/connections/shop.toml"),
        "schema_version = 1\nname = \"Shop\"\nengine = \"sqlite\"\ndatabase = \"${user.shop_file}\"\n\n[mcp]\nexposed = true\n",
    )
    .unwrap();
    fx.spelunk(&["trust", fx.repo().to_str().unwrap()]);
    let query = json!({ "connection": "shop", "sql": "select count(*) as n from products" });

    let mut agent = Agent::start(&fx);
    let (text, is_error) = agent.call("run_query", query.clone());
    assert!(
        is_error && text.contains("${user.shop_file}") && text.contains("user.toml"),
        "{text}"
    );
    agent.stop();

    // Defined in user.toml, the agent can use it.
    std::fs::write(
        fx.user().join("user.toml"),
        format!(
            "schema_version = 1\n\n[vars]\nshop_file = {:?}\n",
            fx.repo().join("shop.db").display().to_string()
        ),
    )
    .unwrap();
    let mut agent = Agent::start(&fx);
    let (text, is_error) = agent.call("run_query", query);
    assert!(!is_error && text.contains("| 3 |"), "{text}");
    agent.stop();
}
