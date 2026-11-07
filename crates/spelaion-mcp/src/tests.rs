//! The server's own decisions, end to end on SQLite files: the user's read-only mode for
//! every agent (ADR-033) and the output format of queries. Each test tries to get around the
//! read-only mode the way an agent or a repository could.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmcp::model::CallToolResult;
use spelaion_config::TrustStore;
use spelaion_store::{Decision, Store, ToolOutput};

use super::*;

/// A fresh directory for one test.
fn temp(name: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let dir = std::env::temp_dir().join(format!(
        "spelaion-mcp-{name}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A SQLite database with a `notes` table and one row.
fn database(dir: &Path) -> PathBuf {
    let path = dir.join("notes.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("create table notes (id integer primary key, body text); insert into notes values (1, 'first');")
        .unwrap();
    path
}

fn rows(db: &Path) -> i64 {
    rusqlite::Connection::open(db)
        .unwrap()
        .query_row("select count(*) from notes", [], |r| r.get(0))
        .unwrap()
}

/// A connection file for `db` that lets agents write.
fn writable_connection(db: &Path, extra: &str) -> String {
    format!(
        "schema_version = 1\nname = \"Notes\"\nengine = \"sqlite\"\ndatabase = '{}'\n{extra}\n[mcp]\nexposed = true\nwrites = \"allow\"\n",
        db.display()
    )
}

fn server(workspace: Option<&Path>, user_dir: &Path, read_only: bool) -> SpelaionMcp {
    SpelaionMcp::new(ServerOptions {
        workspace: workspace.map(Path::to_path_buf),
        user_dir: user_dir.to_path_buf(),
        timeout: Duration::from_secs(10),
        read_only,
    })
    .unwrap()
}

/// The app's handle on the same local store, as when the user flips the switch.
fn app_store(user_dir: &Path) -> Store {
    Store::open(&spelaion_config::state_db(user_dir)).unwrap()
}

fn body(result: &CallToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| c.as_text())
        .map(|t| t.text.clone())
        .collect()
}

fn call(tool: &'static str) -> Call {
    Call {
        client: "test".into(),
        tool,
    }
}

fn refused(result: &CallToolResult) -> bool {
    result.is_error == Some(true)
}

/// A personal connection whose user lets agents write, and a server already running.
struct Personal {
    user_dir: PathBuf,
    db: PathBuf,
    server: SpelaionMcp,
}

fn personal(read_only_flag: bool) -> Personal {
    let user_dir = temp("user");
    let db = database(&user_dir);
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    std::fs::write(
        user_dir.join("connections/notes.toml"),
        writable_connection(&db, ""),
    )
    .unwrap();
    let server = server(None, &user_dir, read_only_flag);
    Personal {
        user_dir,
        db,
        server,
    }
}

const INSERT: &str = "insert into notes (body) values ('from an agent')";

#[tokio::test]
async fn the_switch_takes_writes_away_from_a_running_server() {
    let p = personal(false);
    // As before ADR-033: the connection lets agents write.
    let done = p
        .server
        .write(&call("execute_write"), "~/notes", INSERT)
        .await
        .unwrap();
    assert!(!refused(&done), "{}", body(&done));
    assert_eq!(rows(&p.db), 2);

    // The user turns read-only mode on in the app while the server keeps running.
    app_store(&p.user_dir).set_mcp_read_only(true).unwrap();
    let result = p
        .server
        .write(&call("execute_write"), "~/notes", INSERT)
        .await
        .unwrap();
    assert!(refused(&result));
    assert!(
        body(&result).contains("every agent read-only"),
        "{}",
        body(&result)
    );
    assert_eq!(rows(&p.db), 2);

    // The refusal is in the audit log.
    let last = &app_store(&p.user_dir).recent(1).unwrap()[0];
    assert_eq!(last.decision, Decision::Rejected);
    assert_eq!(last.statement, INSERT);

    // Reads keep working, and a write cannot come in through them either.
    let read = p
        .server
        .read(
            &call("run_query"),
            "~/notes",
            "select count(*) as n from notes",
            None,
        )
        .await
        .unwrap();
    assert!(!refused(&read), "{}", body(&read));
    for sql in [
        INSERT,
        "with x as (insert into notes (body) values ('x') returning id) select * from x",
        "select 1; insert into notes (body) values ('x')",
    ] {
        let sneaked = p
            .server
            .read(&call("run_query"), "~/notes", sql, None)
            .await
            .unwrap();
        assert!(refused(&sneaked), "{sql}: {}", body(&sneaked));
    }
    assert_eq!(rows(&p.db), 2);

    // The agent is told before it tries.
    let listed = p.server.list_connections_inner().await.unwrap();
    assert!(
        body(&listed).contains("every agent read-only"),
        "{}",
        body(&listed)
    );
    assert!(!body(&listed).contains("execute_write is allowed"));

    // Turning it off gives back exactly what the connection allowed.
    app_store(&p.user_dir).set_mcp_read_only(false).unwrap();
    let done = p
        .server
        .write(&call("execute_write"), "~/notes", INSERT)
        .await
        .unwrap();
    assert!(!refused(&done), "{}", body(&done));
    assert_eq!(rows(&p.db), 3);
}

#[tokio::test]
async fn a_trusted_repository_cannot_give_agents_writes_back() {
    let user_dir = temp("repo-user");
    let repo = temp("repo");
    let db = database(&repo);
    let connections = repo.join(".spelaion/connections");
    std::fs::create_dir_all(&connections).unwrap();
    // The team's connection: Production, writes allowed, and fields that try to look like an
    // override of the user's setting are not part of the format.
    std::fs::write(
        connections.join("orders.toml"),
        writable_connection(&db, "env = \"prod\"\nread_only = false\n"),
    )
    .unwrap();
    let config = spelaion_config::load(Some(&repo), &user_dir);
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    TrustStore::load(&user_dir).unwrap().trust(&config).unwrap();
    let store = app_store(&user_dir);
    store.set_mcp_read_only(true).unwrap();

    let server = server(Some(&repo), &user_dir, false);
    let listed = body(&server.list_connections_inner().await.unwrap());
    assert!(listed.contains("- orders"), "{listed}");
    assert!(listed.contains("every agent read-only"), "{listed}");
    let result = server
        .write(&call("execute_write"), "orders", INSERT)
        .await
        .unwrap();
    assert!(refused(&result));
    assert!(body(&result).contains("every agent read-only"));
    assert_eq!(rows(&db), 1);

    // With the switch off, the trusted repository's choice applies again.
    store.set_mcp_read_only(false).unwrap();
    let done = server
        .write(&call("execute_write"), "orders", INSERT)
        .await
        .unwrap();
    assert!(!refused(&done), "{}", body(&done));
    assert_eq!(rows(&db), 2);
}

#[tokio::test]
async fn the_command_line_flag_only_takes_writes_away() {
    let p = personal(true);
    assert!(!app_store(&p.user_dir).mcp_read_only().unwrap());
    let result = p
        .server
        .write(&call("execute_write"), "~/notes", INSERT)
        .await
        .unwrap();
    assert!(refused(&result));
    assert!(body(&result).contains("every agent read-only"));
    assert_eq!(rows(&p.db), 1);
}

#[tokio::test]
async fn without_a_readable_setting_every_agent_is_read_only() {
    let p = personal(false);
    // The store goes away (it cannot be read): agents only read.
    p.server.state.store.lock().unwrap().take();
    let result = p
        .server
        .write(&call("execute_write"), "~/notes", INSERT)
        .await
        .unwrap();
    assert!(refused(&result));
    assert_eq!(rows(&p.db), 1);
}

#[tokio::test]
async fn queries_come_as_json_or_toon() {
    let p = personal(false);
    let sql = "select id, body from notes order by id";
    // JSON until the user picks another default.
    let json = body(
        &p.server
            .read(&call("run_query"), "~/notes", sql, None)
            .await
            .unwrap(),
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed["rows"], serde_json::json!([[1, "first"]]));
    assert_eq!(parsed["columns"][1]["name"], "body");

    app_store(&p.user_dir)
        .set_mcp_output_format(ToolOutput::Toon)
        .unwrap();
    let toon = body(
        &p.server
            .read(&call("run_query"), "~/notes", sql, None)
            .await
            .unwrap(),
    );
    assert!(toon.contains("rows[1]{id,body}:\n  1,first\n"), "{toon}");

    // A call may ask for the other format.
    let json = body(
        &p.server
            .read(&call("run_query"), "~/notes", sql, Some(OutputFormat::Json))
            .await
            .unwrap(),
    );
    assert!(json.starts_with("{\"columns\":"), "{json}");
    let toon = body(
        &p.server
            .read(&call("run_query"), "~/notes", sql, Some(OutputFormat::Toon))
            .await
            .unwrap(),
    );
    assert!(toon.starts_with("columns[2]{name,type}:"), "{toon}");

    // The argument accepts only the two names.
    let args: Result<QueryArgs, _> = serde_json::from_value(serde_json::json!({
        "connection": "~/notes", "sql": sql, "output_format": "markdown"
    }));
    assert!(args.is_err());
}

#[tokio::test]
async fn every_call_is_recorded_with_its_tool_and_session() {
    let p = personal(false);
    p.server
        .read(&call("run_query"), "~/notes", "select 1", None)
        .await
        .unwrap();
    p.server
        .write(&call("execute_write"), "~/notes", "drop table notes")
        .await
        .unwrap();
    let listed = p
        .server
        .audited(
            call("list_objects"),
            "~/notes",
            "schemas".into(),
            p.server.list_objects_inner(Parameters(ListObjectsArgs {
                connection: "~/notes".into(),
                schema: None,
                filter: None,
            })),
        )
        .await
        .unwrap();
    assert!(!refused(&listed), "{}", body(&listed));
    let missing = p
        .server
        .audited(
            call("describe_table"),
            "~/nowhere",
            "main.x".into(),
            p.server.describe_table_inner(Parameters(DescribeArgs {
                connection: "~/nowhere".into(),
                schema: "main".into(),
                table: "x".into(),
            })),
        )
        .await
        .unwrap();
    assert!(refused(&missing));

    let entries = app_store(&p.user_dir).recent(10).unwrap();
    let tools: Vec<_> = entries
        .iter()
        .rev()
        .map(|e| e.tool.clone().unwrap())
        .collect();
    assert_eq!(
        tools,
        [
            "run_query",
            "execute_write",
            "list_objects",
            "describe_table"
        ]
    );
    let decisions: Vec<_> = entries.iter().rev().map(|e| e.decision).collect();
    assert_eq!(
        decisions,
        [
            Decision::Executed,
            Decision::Rejected,
            Decision::Executed,
            Decision::Rejected
        ]
    );
    // One process, one session, the same for every call.
    let session = entries[0].session.clone().unwrap();
    assert!(
        entries
            .iter()
            .all(|e| e.session.as_deref() == Some(session.as_str()))
    );
    assert!(
        session.ends_with(&format!("-{}", std::process::id())),
        "{session}"
    );
    // Another server is another session.
    let other = server(None, &p.user_dir, false);
    other
        .read(&call("run_query"), "~/notes", "select 2", None)
        .await
        .unwrap();
    let latest = &app_store(&p.user_dir).recent(1).unwrap()[0];
    assert_ne!(latest.session.as_deref(), Some(session.as_str()));
}
