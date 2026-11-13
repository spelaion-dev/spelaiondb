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

/// What `list_connections` answers, as a new call of a running server.
async fn listed(server: &SpelaionMcp) -> String {
    let access = server.begin().await;
    body(&server.list_connections_inner(&access).await.unwrap())
}

/// `run_query` as a new call.
async fn query(server: &SpelaionMcp, connection: &str, sql: &str) -> CallToolResult {
    server
        .read(
            &call("run_query"),
            connection,
            sql,
            Some(OutputFormat::Json),
        )
        .await
        .unwrap()
}

/// `execute_write` as a new call.
async fn write(server: &SpelaionMcp, connection: &str, sql: &str) -> CallToolResult {
    server
        .write(&call("execute_write"), connection, sql)
        .await
        .unwrap()
}

/// Rewrites a connection file while the server keeps running.
fn rewrite(path: &Path, content: &str) {
    std::fs::write(path, content).unwrap();
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
async fn plans_return_the_core_tree_and_cache_complete_pages_without_rerunning() {
    let Ok(address) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: MCP plans not tested");
        return;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
    let dir = temp("plans");
    std::fs::create_dir_all(dir.join("connections")).unwrap();
    let config = format!(
        "schema_version=1\nname='Plans'\nengine='postgres'\nhost='{host}'\nport={port}\nuser='spelaion'\ndatabase='spelaion'\n[mcp]\nexposed=true\n"
    );
    std::fs::write(dir.join("connections/plans.toml"), config).unwrap();
    let other = std::fs::read_to_string(dir.join("connections/plans.toml"))
        .unwrap()
        .replace("name='Plans'", "name='Other'");
    std::fs::write(dir.join("connections/other.toml"), other).unwrap();
    let server = server(None, &dir, true);
    // Keep the test password in the owned resolved session, without mutating process env.
    let access = server.begin().await;
    let (snapshot, entry) = exposed_entry(&access, "~/plans").unwrap();
    let definition = snapshot.definition(entry);
    let mut resolved = Resolver::new(&snapshot.config, &snapshot.trust, &SystemBackend)
        .resolve(&entry.id)
        .unwrap();
    resolved.password =
        spelaion_config::Password::Value(spelaion_driver_api::Secret::new("spelaion"));
    resolved.params.read_only = true;
    let open = open_connection(
        resolved,
        &server.state.engines,
        OpenOptions {
            tunnels: server.state.tunnels.clone(),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: dir.join("known_hosts"),
            },
            interaction: Arc::new(NonInteractive),
            password: None,
            read_only_session: true,
        },
        Arc::new(ResultStore::new()),
    )
    .await
    .unwrap();
    // Subsequent pages read PlanStore and create no second execution/audit record.
    server
        .state
        .sessions
        .lock()
        .await
        .insert(("~/plans".into(), Use::Read), Cached::new(open, definition));
    let arguments = |sql: Option<&str>, id| ExplainArgs {
        connection: "~/plans".into(),
        sql: sql.map(str::to_owned),
        structured: Some(true),
        output_format: None,
        plan_id: id,
        offset: None,
        limit: Some(1),
        release: None,
    };
    let result = server
        .explain(
            &call("explain_query"),
            arguments(Some("SELECT 1 UNION ALL SELECT 2"), None),
        )
        .await
        .unwrap();
    assert!(!refused(&result), "{}", body(&result));
    let json: serde_json::Value = serde_json::from_str(&body(&result)).unwrap();
    let id = json["summary"]["id"].as_u64().unwrap() as u32;
    let mut wrong_owner = arguments(None, Some(id));
    wrong_owner.connection = "~/other".into();
    assert!(refused(
        &server
            .explain(&call("explain_query"), wrong_owner)
            .await
            .unwrap()
    ));
    assert_eq!(json["summary"]["engine"], "postgres");
    assert_eq!(json["summary"]["analyzed"], false);
    assert_eq!(json["page"]["nodes"].as_array().unwrap().len(), 1);
    let expected = server
        .state
        .plans
        .tool_json(id, 0, 1, format::MAX_RESPONSE)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&expected).unwrap(),
        json
    );
    let next = json["page"]["nextOffset"].as_u64().unwrap() as u32;
    let mut page = arguments(None, Some(id));
    page.offset = Some(next);
    let page = server.explain(&call("explain_query"), page).await.unwrap();
    assert!(!refused(&page));
    assert!(body(&page).len() <= format::MAX_RESPONSE);
    let audit = app_store(&dir).recent(20).unwrap();
    assert_eq!(
        audit
            .iter()
            .filter(|e| e.statement.starts_with("SELECT 1 UNION"))
            .count(),
        1
    );
    assert!(audit.iter().any(|e| {
        e.detail
            .as_deref()
            .is_some_and(|d| d.contains("no database statement executed"))
    }));
    for sql in [
        "EXPLAIN ANALYZE SELECT 1",
        "SELECT 1; DELETE FROM t",
        "UPDATE t SET v=1",
        "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
    ] {
        assert!(
            refused(
                &server
                    .explain(&call("explain_query"), arguments(Some(sql), None))
                    .await
                    .unwrap()
            ),
            "{sql}"
        );
    }
    assert!(
        serde_json::from_value::<ExplainArgs>(
            serde_json::json!({"connection":"~/plans","sql":"SELECT 1","analyze":true})
        )
        .is_err()
    );
    let mut release = arguments(None, Some(id));
    release.release = Some(true);
    assert!(!refused(
        &server
            .explain(&call("explain_query"), release)
            .await
            .unwrap()
    ));
    assert!(refused(
        &server
            .explain(&call("explain_query"), arguments(None, Some(id)))
            .await
            .unwrap()
    ));
    let mut cached = Vec::new();
    for _ in 0..32 {
        let result = server
            .explain(&call("explain_query"), arguments(Some("SELECT 1"), None))
            .await
            .unwrap();
        assert!(!refused(&result), "{}", body(&result));
        cached.push(
            serde_json::from_str::<serde_json::Value>(&body(&result)).unwrap()["summary"]["id"]
                .as_u64()
                .unwrap() as u32,
        );
    }
    assert!(refused(
        &server
            .explain(&call("explain_query"), arguments(Some("SELECT 1"), None))
            .await
            .unwrap()
    ));
    assert_eq!(server.state.plan_connections.lock().unwrap().len(), 32);
    let mut release = arguments(None, Some(cached[0]));
    release.release = Some(true);
    server
        .explain(&call("explain_query"), release)
        .await
        .unwrap();
    assert_eq!(server.state.plan_connections.lock().unwrap().len(), 31);
    assert!(!refused(
        &server
            .explain(&call("explain_query"), arguments(Some("SELECT 1"), None))
            .await
            .unwrap()
    ));
    assert_eq!(server.state.plan_connections.lock().unwrap().len(), 32);
}

#[tokio::test]
async fn server_plans_return_the_core_tree_and_never_analyze() {
    for (variable, engine) in [
        ("SPELAION_TEST_MYSQL", "mysql"),
        ("SPELAION_TEST_MARIADB", "mariadb"),
        ("SPELAION_TEST_MSSQL", "sqlserver"),
    ] {
        let Ok(address) = std::env::var(variable) else {
            eprintln!("{variable} not set: MCP {engine} plans not tested");
            continue;
        };
        let (host, port) = address.rsplit_once(':').unwrap();
        let (user, database, password) = if engine == "sqlserver" {
            ("sa", "master", "Spelaion-test-1")
        } else {
            ("spelaion", "spelaion", "spelaion")
        };
        let dir = temp(&format!("plans-{engine}"));
        std::fs::create_dir_all(dir.join("connections")).unwrap();
        std::fs::write(
            dir.join("connections/plans.toml"),
            format!(
                "schema_version=1\nname='Plans'\nengine='{engine}'\nhost='{host}'\nport={port}\nuser='{user}'\ndatabase='{database}'\n[tls]\nmode='require'\n[mcp]\nexposed=true\n"
            ),
        )
        .unwrap();
        let server = server(None, &dir, true);
        let access = server.begin().await;
        let (snapshot, entry) = exposed_entry(&access, "~/plans").unwrap();
        let definition = snapshot.definition(entry);
        let mut resolved = Resolver::new(&snapshot.config, &snapshot.trust, &SystemBackend)
            .resolve(&entry.id)
            .unwrap();
        resolved.password =
            spelaion_config::Password::Value(spelaion_driver_api::Secret::new(password));
        resolved.params.read_only = true;
        let open = open_connection(
            resolved,
            &server.state.engines,
            OpenOptions {
                tunnels: server.state.tunnels.clone(),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: dir.join("known_hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: None,
                read_only_session: true,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .unwrap();
        server
            .state
            .sessions
            .lock()
            .await
            .insert(("~/plans".into(), Use::Read), Cached::new(open, definition));
        let arguments = |sql: &str| ExplainArgs {
            connection: "~/plans".into(),
            sql: Some(sql.into()),
            structured: Some(true),
            output_format: None,
            plan_id: None,
            offset: None,
            limit: Some(50),
            release: None,
        };
        let result = server
            .explain(
                &call("explain_query"),
                arguments("SELECT 1 AS a UNION ALL SELECT 2"),
            )
            .await
            .unwrap();
        assert!(!refused(&result), "{engine}: {}", body(&result));
        let json: serde_json::Value = serde_json::from_str(&body(&result)).unwrap();
        assert_eq!(json["summary"]["engine"], engine);
        assert_eq!(json["summary"]["analyzed"], false);
        assert!(!json["page"]["nodes"].as_array().unwrap().is_empty());
        for sql in [
            "EXPLAIN ANALYZE SELECT 1",
            "ANALYZE FORMAT=JSON SELECT 1",
            "SELECT 1; DELETE FROM t",
            "UPDATE t SET v=1",
            "DELETE FROM t",
        ] {
            assert!(
                refused(
                    &server
                        .explain(&call("explain_query"), arguments(sql))
                        .await
                        .unwrap()
                ),
                "{engine}: {sql}"
            );
        }
    }
}

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
    let listed = listed(&p.server).await;
    assert!(listed.contains("every agent read-only"), "{listed}");
    assert!(!listed.contains("execute_write is allowed"));

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
    let listed = listed(&server).await;
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
    let access = p.server.begin().await;
    let listed = p
        .server
        .audited(
            call("list_objects"),
            &access,
            "~/notes",
            "schemas".into(),
            p.server.list_objects_inner(
                &access,
                Parameters(ListObjectsArgs {
                    connection: "~/notes".into(),
                    schema: None,
                    filter: None,
                }),
            ),
        )
        .await
        .unwrap();
    assert!(!refused(&listed), "{}", body(&listed));
    let access = p.server.begin().await;
    let missing = p
        .server
        .audited(
            call("describe_table"),
            &access,
            "~/nowhere",
            "main.x".into(),
            p.server.describe_table_inner(
                &access,
                Parameters(DescribeArgs {
                    connection: "~/nowhere".into(),
                    schema: "main".into(),
                    table: "x".into(),
                }),
            ),
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

#[tokio::test]
async fn an_agents_read_session_refuses_a_setup_that_writes() {
    let user_dir = temp("setup");
    let db = database(&user_dir);
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    // The user's own setup writes: their editor runs it, an agent's read session must not.
    std::fs::write(
        user_dir.join("connections/notes.toml"),
        writable_connection(
            &db,
            "\n[session]\nsetup = \"insert into notes (body) values ('setup')\"\nignore_setup_errors = true\n",
        ),
    )
    .unwrap();
    let mcp = server(None, &user_dir, false);
    let result = mcp
        .read(
            &call("run_query"),
            "~/notes",
            "select count(*) from notes",
            None,
        )
        .await
        .unwrap();
    assert!(refused(&result), "{}", body(&result));
    assert!(body(&result).contains("session setup"), "{}", body(&result));
    assert_eq!(rows(&db), 1);

    // A setup that only reads or sets the session is fine for agents.
    std::fs::write(
        user_dir.join("connections/notes.toml"),
        writable_connection(&db, "\n[session]\nsetup = \"select count(*) from notes\"\n"),
    )
    .unwrap();
    let mcp = server(None, &user_dir, false);
    let result = mcp
        .read(
            &call("run_query"),
            "~/notes",
            "select count(*) from notes",
            None,
        )
        .await
        .unwrap();
    assert!(!refused(&result), "{}", body(&result));
    assert_eq!(rows(&db), 1);
}

/// Column masking (ADR-051) on an agent's query: the masked column's values never reach the
/// agent, whatever the output format, whether the rule is the connection's, the user's or a
/// connection type's, and the connection's `never` cannot unmask what the type requires.
#[tokio::test]
async fn agents_get_the_mask_never_the_value() {
    let user_dir = temp("masking");
    let db = user_dir.join("people.db");
    rusqlite::Connection::open(&db)
        .unwrap()
        .execute_batch(
            "create table people (id integer primary key, email text, card text, ssn text);
             insert into people values (1, 'ada@example.com', '4111111111111111', '123-45-6789'),
                                       (2, null, '5500000000000004', '987-65-4321');",
        )
        .unwrap();
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    std::fs::write(
        user_dir.join("connection-types.toml"),
        "schema_version = 1\n[types.pii]\nname = \"PII\"\nmask_columns = [\"ssn\"]\n",
    )
    .unwrap();
    std::fs::write(
        user_dir.join("user.toml"),
        "schema_version = 1\n[masking]\nalways = [\"card\"]\n",
    )
    .unwrap();
    std::fs::write(
        user_dir.join("connections/people.toml"),
        format!(
            "schema_version = 1\nname = \"People\"\nengine = \"sqlite\"\nenv = \"pii\"\ndatabase = '{}'\n[mcp]\nexposed = true\n[masking]\nalways = [\"*email*\"]\nnever = [\"ssn\"]\n",
            db.display()
        ),
    )
    .unwrap();
    let server = server(None, &user_dir, false);
    let leaks = [
        "ada@example.com",
        "4111111111111111",
        "123-45-6789",
        "987-65-4321",
    ];
    for sql in [
        "select * from people order by id",
        "select email as contact_email, card, ssn from people",
        // ADR-052: SQLite says which table column each result column reads.
        "select email as e, card as c, ssn as s from people",
        "select id, ssn from people where email = 'ada@example.com'",
    ] {
        for format in [OutputFormat::Json, OutputFormat::Toon] {
            let text = body(
                &server
                    .read(&call("run_query"), "~/people", sql, Some(format))
                    .await
                    .unwrap(),
            );
            for leak in leaks {
                assert!(!text.contains(leak), "{sql} leaked {leak}: {text}");
            }
            assert!(text.contains(spelaion_core::MASK), "{text}");
        }
    }
    let json = body(
        &server
            .read(
                &call("run_query"),
                "~/people",
                "select id, email, card, ssn from people order by id",
                Some(OutputFormat::Json),
            )
            .await
            .unwrap(),
    );
    let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
    // NULL stays NULL; the id is not masked; every masked column says so.
    assert_eq!(
        parsed["rows"],
        serde_json::json!([
            [1, "••••••", "••••••", "••••••"],
            [2, null, "••••••", "••••••"]
        ])
    );
    assert_eq!(parsed["columns"][0].get("masked"), None);
    for c in 1..4 {
        assert_eq!(parsed["columns"][c]["masked"], true, "{json}");
    }
}

// A running server and the user's changes (SDB-119): what agents may do is decided again at
// every call, from the files as they are then, through the same checks as at start-up.

/// A personal connection file for `db` with its `[mcp]` table.
fn connection(db: &Path, mcp: &str) -> String {
    format!(
        "schema_version = 1\nname = \"Notes\"\nengine = \"sqlite\"\ndatabase = '{}'\n[mcp]\n{mcp}\n",
        db.display()
    )
}

#[tokio::test]
async fn hiding_a_connection_takes_effect_at_the_next_call() {
    let p = personal(false);
    let file = p.user_dir.join("connections/notes.toml");
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));
    assert!(!p.server.state.sessions.lock().await.is_empty());

    rewrite(
        &file,
        &connection(&p.db, "exposed = false\nwrites = \"allow\""),
    );
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(refused(&read));
    assert!(body(&read).contains("not exposed"), "{}", body(&read));
    // Its cached session is closed, not kept for later.
    assert!(p.server.state.sessions.lock().await.is_empty());
    let written = write(&p.server, "~/notes", INSERT).await;
    assert!(refused(&written));
    assert_eq!(rows(&p.db), 1);
    assert!(!listed(&p.server).await.contains("~/notes"));
    let access = p.server.begin().await;
    let answer = p
        .server
        .audited(
            call("list_objects"),
            &access,
            "~/notes",
            "schemas".into(),
            p.server.list_objects_inner(
                &access,
                Parameters(ListObjectsArgs {
                    connection: "~/notes".into(),
                    schema: None,
                    filter: None,
                }),
            ),
        )
        .await
        .unwrap();
    assert!(refused(&answer), "{}", body(&answer));
    let last = &app_store(&p.user_dir).recent(1).unwrap()[0];
    assert_eq!(last.decision, Decision::Rejected);
}

#[tokio::test]
async fn denying_writes_refuses_the_next_write_and_records_it() {
    let p = personal(false);
    let file = p.user_dir.join("connections/notes.toml");
    let done = write(&p.server, "~/notes", INSERT).await;
    assert!(!refused(&done), "{}", body(&done));
    assert_eq!(rows(&p.db), 2);

    rewrite(
        &file,
        &connection(&p.db, "exposed = true\nwrites = \"deny\""),
    );
    let result = write(&p.server, "~/notes", INSERT).await;
    assert!(refused(&result));
    assert!(
        body(&result).contains("read-only for agents"),
        "{}",
        body(&result)
    );
    assert_eq!(rows(&p.db), 2);
    let last = &app_store(&p.user_dir).recent(1).unwrap()[0];
    assert_eq!(last.decision, Decision::Rejected);
    assert_eq!(last.statement, INSERT);
    assert!(listed(&p.server).await.contains("Read-only."));

    // Making the connection read-only refuses it too, whatever `writes` says.
    rewrite(&file, &writable_connection(&p.db, "read_only = true\n"));
    let result = write(&p.server, "~/notes", INSERT).await;
    assert!(refused(&result));
    assert!(
        body(&result).contains("for agents and for the user"),
        "{}",
        body(&result)
    );
    assert_eq!(rows(&p.db), 2);

    // Allowing writes again goes through the same checks: writes come back.
    rewrite(&file, &writable_connection(&p.db, ""));
    let done = write(&p.server, "~/notes", INSERT).await;
    assert!(!refused(&done), "{}", body(&done));
    assert_eq!(rows(&p.db), 3);
}

#[tokio::test]
async fn a_lower_max_rows_applies_at_the_next_call() {
    let p = personal(false);
    let file = p.user_dir.join("connections/notes.toml");
    for _ in 0..4 {
        let done = write(&p.server, "~/notes", INSERT).await;
        assert!(!refused(&done), "{}", body(&done));
    }
    assert_eq!(rows(&p.db), 5);
    let sql = "select id from notes order by id";
    let all: serde_json::Value =
        serde_json::from_str(&body(&query(&p.server, "~/notes", sql).await)).unwrap();
    assert_eq!(all["rows"].as_array().unwrap().len(), 5);

    rewrite(
        &file,
        &connection(&p.db, "exposed = true\nwrites = \"allow\"\nmax_rows = 2"),
    );
    let some: serde_json::Value =
        serde_json::from_str(&body(&query(&p.server, "~/notes", sql).await)).unwrap();
    assert_eq!(some["rows"].as_array().unwrap().len(), 2, "{some}");
    assert_eq!(some["more_rows"], true, "{some}");
}

#[tokio::test]
async fn a_changed_definition_opens_a_new_session() {
    let p = personal(false);
    let file = p.user_dir.join("connections/notes.toml");
    let other = p.user_dir.join("other.db");
    rusqlite::Connection::open(&other)
        .unwrap()
        .execute_batch(
            "create table notes (id integer primary key, body text); insert into notes values (1, 'elsewhere');",
        )
        .unwrap();
    let sql = "select body from notes order by id";
    assert!(body(&query(&p.server, "~/notes", sql).await).contains("first"));

    // The same connection now points at another file: the cached session is not reused.
    rewrite(&file, &writable_connection(&other, ""));
    let read = body(&query(&p.server, "~/notes", sql).await);
    assert!(read.contains("elsewhere"), "{read}");
    assert!(!read.contains("first"), "{read}");

    // A change outside the connection's file that it is opened with (`user.toml`'s masking)
    // reopens it too.
    std::fs::write(
        p.user_dir.join("user.toml"),
        "schema_version = 1\n[masking]\nalways = [\"body\"]\n",
    )
    .unwrap();
    let read = body(&query(&p.server, "~/notes", sql).await);
    assert!(!read.contains("elsewhere"), "{read}");
    assert!(read.contains(spelaion_core::MASK), "{read}");
}

#[tokio::test]
async fn removing_trust_hides_the_repositorys_connections() {
    let user_dir = temp("trust-user");
    let repo = temp("trust-repo");
    let db = database(&repo);
    let connections = repo.join(".spelaion/connections");
    std::fs::create_dir_all(&connections).unwrap();
    std::fs::write(
        connections.join("orders.toml"),
        writable_connection(&db, ""),
    )
    .unwrap();
    let config = spelaion_config::load(Some(&repo), &user_dir);
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    let root = config.workspace_root.clone().unwrap();
    TrustStore::load(&user_dir).unwrap().trust(&config).unwrap();
    let server = server(Some(&repo), &user_dir, false);
    assert!(listed(&server).await.contains("- orders"));
    let read = query(&server, "orders", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));

    // The user removes the repository's trust in the app.
    TrustStore::load(&user_dir).unwrap().revoke(&root).unwrap();
    let list = listed(&server).await;
    assert!(!list.contains("- orders"), "{list}");
    assert!(list.contains("is not trusted"), "{list}");
    let read = query(&server, "orders", "select count(*) from notes").await;
    assert!(refused(&read));
    assert!(body(&read).contains("not exposed"), "{}", body(&read));
    assert!(refused(&write(&server, "orders", INSERT).await));
    assert!(server.state.sessions.lock().await.is_empty());
    assert_eq!(rows(&db), 1);

    // Trusted again, it comes back. A change to its file after that trust is then refused by
    // the trust pin, exactly as at start-up (ADR-022), although a session was open.
    let config = spelaion_config::load(Some(&repo), &user_dir);
    TrustStore::load(&user_dir).unwrap().trust(&config).unwrap();
    let read = query(&server, "orders", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));
    let elsewhere = repo.join("elsewhere.db");
    std::fs::copy(&db, &elsewhere).unwrap();
    std::fs::write(
        connections.join("orders.toml"),
        writable_connection(&elsewhere, ""),
    )
    .unwrap();
    let read = query(&server, "orders", "select count(*) from notes").await;
    assert!(refused(&read), "{}", body(&read));
    let written = write(&server, "orders", INSERT).await;
    assert!(refused(&written), "{}", body(&written));
    assert_eq!(rows(&elsewhere), 1);
    assert_eq!(rows(&db), 1);
}

#[tokio::test]
async fn a_broken_file_exposes_nothing_until_it_is_fixed() {
    let p = personal(false);
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));

    // Another file breaks (half written, or a typo): nothing is exposed, and the agent is told
    // why, without the file's contents.
    let broken = p.user_dir.join("connections/half.toml");
    std::fs::write(
        &broken,
        "schema_version = 1\nname = \"secret-ish value\nengine =",
    )
    .unwrap();
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(refused(&read));
    let reason = body(&read);
    assert!(reason.contains("configuration has problems"), "{reason}");
    assert!(reason.contains("half.toml"), "{reason}");
    assert!(!reason.contains("secret-ish"), "{reason}");
    assert!(p.server.state.sessions.lock().await.is_empty());
    let written = write(&p.server, "~/notes", INSERT).await;
    assert!(refused(&written));
    assert_eq!(rows(&p.db), 1);
    let last = &app_store(&p.user_dir).recent(1).unwrap()[0];
    assert_eq!(last.decision, Decision::Rejected);
    let access = p.server.begin().await;
    let list = p
        .server
        .audited(
            call("list_connections"),
            &access,
            "",
            "list_connections".into(),
            p.server.list_connections_inner(&access),
        )
        .await
        .unwrap();
    assert!(refused(&list));
    assert!(!body(&list).contains("- ~/notes"), "{}", body(&list));
    let last = &app_store(&p.user_dir).recent(1).unwrap()[0];
    assert_eq!(last.decision, Decision::Rejected);

    // A broken `user.toml` too.
    std::fs::remove_file(&broken).unwrap();
    std::fs::write(p.user_dir.join("user.toml"), "schema_version = = 1").unwrap();
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(refused(&read), "{}", body(&read));

    // Fixed: served again, without a restart.
    std::fs::remove_file(p.user_dir.join("user.toml")).unwrap();
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));
}

#[tokio::test]
async fn a_notice_that_only_informs_hides_nothing() {
    let user_dir = temp("notice-user");
    let repo = temp("notice-repo");
    let db = database(&repo);
    let connections = repo.join(".spelaion/connections");
    std::fs::create_dir_all(&connections).unwrap();
    std::fs::write(
        connections.join("orders.toml"),
        writable_connection(&db, ""),
    )
    .unwrap();
    // The old folder is still there: the configuration says so, and nothing else changes.
    std::fs::create_dir_all(repo.join(".spelunk")).unwrap();
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    std::fs::write(
        user_dir.join("connections/notes.toml"),
        writable_connection(&db, ""),
    )
    .unwrap();
    let config = spelaion_config::load(Some(&repo), &user_dir);
    assert!(!config.problems.is_empty());
    assert!(
        config.problems.iter().all(|p| !p.blocks()),
        "{:?}",
        config.problems
    );
    TrustStore::load(&user_dir).unwrap().trust(&config).unwrap();

    let server = server(Some(&repo), &user_dir, false);
    let list = listed(&server).await;
    assert!(list.contains("- orders"), "{list}");
    assert!(list.contains("- ~/notes"), "{list}");
    for id in ["orders", "~/notes"] {
        let read = query(&server, id, "select count(*) from notes").await;
        assert!(!refused(&read), "{id}: {}", body(&read));
    }
    let done = write(&server, "orders", INSERT).await;
    assert!(!refused(&done), "{}", body(&done));

    // A file that does not load still exposes nothing, notice or not.
    std::fs::write(connections.join("half.toml"), "name = = 1").unwrap();
    let read = query(&server, "~/notes", "select count(*) from notes").await;
    assert!(refused(&read));
    let reason = body(&read);
    assert!(reason.contains("half.toml"), "{reason}");
    // The notice is not what blocks.
    assert!(!reason.contains(".spelunk"), "{reason}");
}

#[tokio::test]
async fn a_newly_exposed_connection_appears_without_a_restart() {
    let user_dir = temp("expose");
    let db = database(&user_dir);
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    let file = user_dir.join("connections/notes.toml");
    rewrite(&file, &connection(&db, "exposed = false"));
    let server = server(None, &user_dir, false);
    assert!(listed(&server).await.contains("No exposed connections"));
    assert!(refused(&query(&server, "~/notes", "select 1").await));

    rewrite(&file, &connection(&db, "exposed = true"));
    std::fs::write(
        user_dir.join("connections/more.toml"),
        connection(&db, "exposed = true").replace("Notes", "More"),
    )
    .unwrap();
    let list = listed(&server).await;
    assert!(list.contains("- ~/notes"), "{list}");
    assert!(list.contains("- ~/more"), "{list}");
    let read = query(&server, "~/more", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));
    // Exposed for reading only: writes are still denied by default.
    assert!(refused(&write(&server, "~/notes", INSERT).await));
    assert_eq!(rows(&db), 1);
}

// A lost database session (SDB-118): the call that finds out fails and is never retried; the
// next call opens a new session through the same path as the first one (ADR-028).

/// The cached session of `id` for `purpose`.
async fn cached(server: &SpelaionMcp, id: &str, purpose: Use) -> Shared {
    server
        .state
        .sessions
        .lock()
        .await
        .get(&(id.to_string(), purpose))
        .expect("a cached session")
        .open
        .clone()
}

#[tokio::test]
async fn a_lost_session_is_replaced_at_the_next_call() {
    let p = personal(false);
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));
    let first = cached(&p.server, "~/notes", Use::Read).await;
    // What the core does when the server ends the session, a call meets a connection error or
    // the tunnel goes down.
    first.lock().await.session.mark_lost("the server ended it");
    let read = query(&p.server, "~/notes", "select count(*) from notes").await;
    assert!(!refused(&read), "{}", body(&read));
    let second = cached(&p.server, "~/notes", Use::Read).await;
    assert!(!Arc::ptr_eq(&first, &second));
    // The new read session is opened as the first one was: still read-only.
    let sneaked = query(&p.server, "~/notes", INSERT).await;
    assert!(refused(&sneaked));
    assert_eq!(rows(&p.db), 1);

    let done = write(&p.server, "~/notes", INSERT).await;
    assert!(!refused(&done), "{}", body(&done));
    let first = cached(&p.server, "~/notes", Use::Write).await;
    first.lock().await.session.mark_lost("the server ended it");
    let done = write(&p.server, "~/notes", INSERT).await;
    assert!(!refused(&done), "{}", body(&done));
    assert!(!Arc::ptr_eq(
        &first,
        &cached(&p.server, "~/notes", Use::Write).await
    ));
    assert_eq!(rows(&p.db), 3);
}

/// Runs `body`, then `cleanup` whether it finished or failed an assertion, and only then lets
/// the failure go on (like the core tests' `cleaning_up`).
async fn cleaning_up<T, C>(
    body: impl std::future::Future<Output = T>,
    cleanup: impl FnOnce() -> C,
) -> T
where
    C: std::future::Future<Output = ()>,
{
    use futures_util::FutureExt;
    let outcome = std::panic::AssertUnwindSafe(body).catch_unwind().await;
    cleanup().await;
    match outcome {
        Ok(value) => value,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Runs `sql` on the test's own (not exposed) connection, and fails the test if it fails.
async fn admin_run(admin: &OpenConnection, sql: &str) {
    use futures_util::StreamExt;
    let events: Vec<spelaion_core::CoreEvent> = tokio::time::timeout(
        Duration::from_secs(20),
        admin.session.run_script(sql).collect(),
    )
    .await
    .expect("the admin statement answers");
    for event in &events {
        if let spelaion_core::CoreEvent::StatementFailed { error, .. } = event {
            panic!("{sql}: {}", error.message);
        }
    }
}

/// Ends, from the server's side, every session tagged `tag`, and waits until they are gone.
async fn kill_sessions(admin: &OpenConnection, tag: &str) {
    admin_run(
        admin,
        &format!(
            "select pg_terminate_backend(pid, 5000) from pg_stat_activity where application_name = '{tag}'"
        ),
    )
    .await;
}

/// The value of a one-cell query through the server, allowing the one failure of a call that
/// finds its session lost.
async fn read_value(server: &SpelaionMcp, id: &str, sql: &str) -> serde_json::Value {
    let mut result = query(server, id, sql).await;
    if refused(&result) {
        assert!(
            body(&result).contains("next call opens a new session"),
            "{}",
            body(&result)
        );
        result = query(server, id, sql).await;
    }
    assert!(!refused(&result), "{}", body(&result));
    let json: serde_json::Value = serde_json::from_str(&body(&result)).unwrap();
    json["rows"][0][0].clone()
}

/// The card's scenario on a PostgreSQL connection the server already uses: its sessions are
/// ended by the server, the next call fails (and is recorded as failed) or succeeds, and the
/// call after that succeeds on a new session. A write is never retried.
async fn survives_ended_sessions(
    server: &SpelaionMcp,
    user_dir: &Path,
    admin: &OpenConnection,
    id: &str,
    tag: &str,
    table: &str,
) {
    let pid = read_value(server, id, "select pg_backend_pid()").await;
    kill_sessions(admin, tag).await;
    let next = query(server, id, "select pg_backend_pid()").await;
    if refused(&next) {
        let text = body(&next);
        assert!(text.contains("next call opens a new session"), "{text}");
        let last = &app_store(user_dir).recent(1).unwrap()[0];
        assert_eq!(last.decision, Decision::Failed, "{last:?}");
        assert_eq!(last.statement, "select pg_backend_pid()");
    }
    let after = query(server, id, "select pg_backend_pid()").await;
    assert!(!refused(&after), "{}", body(&after));
    let json: serde_json::Value = serde_json::from_str(&body(&after)).unwrap();
    assert_ne!(json["rows"][0][0], pid, "a new session, not the old one");
    let last = &app_store(user_dir).recent(1).unwrap()[0];
    assert_eq!(last.decision, Decision::Executed);

    // Writes: the one that finds its session lost is never retried; the next one works.
    let insert = format!("insert into {table} (v) values ('agent')");
    let mut answered_done = 0;
    let done = write(server, id, &insert).await;
    assert!(!refused(&done), "{}", body(&done));
    answered_done += 1;
    kill_sessions(admin, tag).await;
    let next = write(server, id, &insert).await;
    if refused(&next) {
        let text = body(&next);
        assert!(
            text.contains("not known whether the write was committed"),
            "{text}"
        );
        let last = &app_store(user_dir).recent(1).unwrap()[0];
        assert_eq!(last.decision, Decision::Failed, "{last:?}");
    } else {
        answered_done += 1;
    }
    let done = write(server, id, &insert).await;
    assert!(!refused(&done), "{}", body(&done));
    answered_done += 1;
    let count = read_value(server, id, &format!("select count(*) from {table}")).await;
    assert_eq!(count, serde_json::json!(answered_done));
}

/// A PostgreSQL connection file for the test server, tagged so that only its sessions are ended.
fn postgres_connection(
    host: &str,
    port: Option<&str>,
    tag: &str,
    extra: &str,
    exposed: bool,
) -> String {
    let port = port.map(|p| format!("port = {p}\n")).unwrap_or_default();
    format!(
        "schema_version = 1\nengine = \"postgres\"\nhost = \"{host}\"\n{port}user = \"spelaion\"\ndatabase = \"spelaion\"\npassword = {{ secret = \"cmd:echo spelaion\" }}\n{extra}\n[session]\nsetup = \"set application_name = '{tag}'\"\n\n[mcp]\nexposed = {exposed}\nwrites = \"allow\"\n"
    )
}

/// Opens the test's own connection `id` (not exposed to agents) as the app would.
async fn admin_connection(server: &SpelaionMcp, id: &str, user_dir: &Path) -> OpenConnection {
    let access = server.begin().await;
    let snapshot = access.as_ref().unwrap();
    let id = spelaion_config::ConnectionId {
        source: Source::User,
        path: id.into(),
    };
    let resolved = Resolver::new(&snapshot.config, &snapshot.trust, &SystemBackend)
        .resolve(&id)
        .unwrap();
    open_connection(
        resolved,
        &server.state.engines,
        OpenOptions {
            tunnels: server.state.tunnels.clone(),
            known_hosts: KnownHosts {
                team_file: None,
                user_file: None,
                app_file: user_dir.join("known_hosts"),
            },
            interaction: Arc::new(NonInteractive),
            password: None,
            read_only_session: false,
        },
        Arc::new(ResultStore::new()),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn postgres_sessions_ended_by_the_server_are_replaced() {
    let Ok(address) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: lost MCP sessions not tested on PostgreSQL");
        return;
    };
    let (host, port) = address.rsplit_once(':').unwrap();
    let user_dir = temp("lost-pg");
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    let n = std::process::id();
    let tag = format!("spelaion-mcp-lost-{n}");
    let table = format!("mcp_lost_{n}");
    std::fs::write(
        user_dir.join("connections/pg.toml"),
        postgres_connection(host, Some(port), &tag, "", true),
    )
    .unwrap();
    std::fs::write(
        user_dir.join("connections/admin.toml"),
        postgres_connection(host, Some(port), "spelaion-mcp-admin", "", false),
    )
    .unwrap();
    let server = server(None, &user_dir, false);
    let admin = admin_connection(&server, "admin", &user_dir).await;
    admin_run(
        &admin,
        &format!(
            "drop table if exists {table}; create table {table} (id serial primary key, v text)"
        ),
    )
    .await;
    cleaning_up(
        survives_ended_sessions(&server, &user_dir, &admin, "~/pg", &tag, &table),
        || async { admin_run(&admin, &format!("drop table if exists {table}")).await },
    )
    .await;
}

/// Accepts the test bastions' host keys once, so the headless server then finds them known.
struct AcceptHosts;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        true
    }

    async fn ask_secret(
        &self,
        _request: &spelaion_ssh::SecretRequest,
    ) -> Option<spelaion_driver_api::Secret> {
        None
    }
}

#[tokio::test]
async fn tunnelled_sessions_ended_by_the_server_are_replaced() {
    let (Ok(ssh), Ok(pg)) = (
        std::env::var("SPELAION_TEST_SSH"),
        std::env::var("SPELAION_TEST_PG"),
    ) else {
        eprintln!(
            "SPELAION_TEST_SSH or SPELAION_TEST_PG not set: lost tunnelled MCP sessions not tested"
        );
        return;
    };
    let (ssh_host, ssh_port) = ssh.rsplit_once(':').unwrap();
    let (pg_host, pg_port) = pg.rsplit_once(':').unwrap();
    let user_dir = temp("lost-tunnel");
    std::fs::create_dir_all(user_dir.join("connections")).unwrap();
    std::fs::create_dir_all(user_dir.join("tunnels")).unwrap();
    let keys = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys");
    let identity = format!("'{}'", keys.join("id_ed25519").display());
    std::fs::write(
        user_dir.join("tunnels/bastions.toml"),
        format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{ssh_host}\"\nport = {ssh_port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    )
    .unwrap();
    let n = std::process::id();
    let tag = format!("spelaion-mcp-lost-tunnel-{n}");
    let table = format!("mcp_lost_tunnel_{n}");
    // Behind the bastions the database is `postgres` on its own port.
    std::fs::write(
        user_dir.join("connections/behind.toml"),
        postgres_connection("postgres", None, &tag, "tunnel = \"bastions\"", true),
    )
    .unwrap();
    std::fs::write(
        user_dir.join("connections/admin.toml"),
        postgres_connection(pg_host, Some(pg_port), "spelaion-mcp-admin", "", false),
    )
    .unwrap();
    let server = server(None, &user_dir, false);

    // The user accepted the bastions' keys once, in the app.
    {
        let access = server.begin().await;
        let snapshot = access.as_ref().unwrap();
        let (_, entry) = exposed_entry(&access, "~/behind").unwrap();
        let resolved = Resolver::new(&snapshot.config, &snapshot.trust, &SystemBackend)
            .resolve(&entry.id)
            .unwrap();
        let learnt = open_connection(
            resolved,
            &server.state.engines,
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_file: None,
                    user_file: None,
                    app_file: user_dir.join("known_hosts"),
                },
                interaction: Arc::new(AcceptHosts),
                password: None,
                read_only_session: true,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .unwrap();
        learnt.close().await;
    }

    let admin = admin_connection(&server, "admin", &user_dir).await;
    admin_run(
        &admin,
        &format!(
            "drop table if exists {table}; create table {table} (id serial primary key, v text)"
        ),
    )
    .await;
    cleaning_up(
        async {
            survives_ended_sessions(&server, &user_dir, &admin, "~/behind", &tag, &table).await;
            // The new sessions still go through the tunnel.
            let open = cached(&server, "~/behind", Use::Read).await;
            assert!(open.lock().await.tunnel_state().is_some());
        },
        || async { admin_run(&admin, &format!("drop table if exists {table}")).await },
    )
    .await;
}
