//! Oracle Database, served only by its plugin on Oracle's thin driver (ADR-059), through the
//! core: scripts split as SQL*Plus reads them (PL/SQL blocks and `/`), the transaction read from
//! the driver, what Oracle does not offer yet (read-only connections and agents), what a user
//! the database keeps read-only refuses and what it does not (ADR-080), and a connection through
//! the two SSH bastions with TLS checked against the database's name.
//!
//! They need `SPELAION_TEST_ORACLE` (`testing/docker`, 127.0.0.1:11521) and the plugin: the
//! pinned build (`spelaion-fetch-plugins`) or one built on this machine
//! (`SPELAION_DEV_PLUGIN_ORACLE`). The tunnel test also needs `SPELAION_TEST_SSH`. Without them
//! the tests are skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::{ConnectionId, Resolver, Source, SystemBackend, TrustStore, load};
use spelaion_core::{
    AgentView, CoreEvent, Engines, OpenConnection, OpenOptions, ResultStore, TunnelPool,
    agent_view, open_connection,
};
use spelaion_driver_api::{Secret, TransactionState, Value};
use spelaion_ssh::KnownHosts;

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_ORACLE") else {
        eprintln!("SPELAION_TEST_ORACLE not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("a port")))
}

/// A temporary, trusted repository whose connections a test writes.
struct Workspace {
    base: PathBuf,
}

impl Workspace {
    fn new(label: &str) -> Self {
        let base = std::env::temp_dir().join(format!("spelaion-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("repo/.spelaion")).unwrap();
        std::fs::create_dir_all(base.join("user")).unwrap();
        Self { base }
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.base.join("repo").join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn config(&self) -> spelaion_config::Config {
        load(Some(&self.base.join("repo")), &self.base.join("user"))
    }

    async fn open(&self, id: &str) -> OpenConnection {
        self.open_with(id, "spelaion").await
    }

    async fn open_with(&self, id: &str, password: &str) -> OpenConnection {
        let config = self.config();
        assert!(config.problems.is_empty(), "{:?}", config.problems);
        let mut trust = TrustStore::load(&self.base.join("user")).unwrap();
        trust.trust(&config).unwrap();
        let resolved = Resolver::new(&config, &trust, &SystemBackend)
            .resolve(&ConnectionId {
                source: Source::Workspace,
                path: id.into(),
            })
            .expect("resolving");
        open_connection(
            resolved,
            &Engines::builtin(),
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_files: Vec::new(),
                    user_file: None,
                    app_file: self.base.join("user/known_hosts"),
                },
                interaction: Arc::new(AcceptHosts),
                password: Some(Secret::new(password)),
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        )
        .await
        .expect("opens")
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

struct AcceptHosts;

#[async_trait::async_trait]
impl spelaion_ssh::Interaction for AcceptHosts {
    async fn confirm_unknown_host(&self, _host: &spelaion_ssh::HostInfo) -> bool {
        true
    }
    async fn ask_secret(&self, _request: &spelaion_ssh::SecretRequest) -> Option<Secret> {
        None
    }
}

fn toml_path(path: &Path) -> String {
    format!("{:?}", path.display().to_string().replace('\\', "/"))
}

fn keys() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../testing/docker/keys")
}

fn connection(host: &str, port: u16, extra: &str) -> String {
    connection_as(host, port, "spelaion", extra)
}

fn connection_as(host: &str, port: u16, user: &str, extra: &str) -> String {
    format!(
        "schema_version = 1\nengine = \"oracle\"\nhost = \"{host}\"\nport = {port}\ndatabase = \"FREEPDB1\"\nuser = \"{user}\"\npassword = {{ secret = \"prompt\" }}\n{extra}"
    )
}

/// Runs a script; the first failure fails the test.
async fn script(open: &OpenConnection, sql: &str) -> Vec<CoreEvent> {
    let events: Vec<CoreEvent> = open.session.run_script(sql).collect().await;
    if let Some(CoreEvent::StatementFailed { error, .. }) = events
        .iter()
        .find(|e| matches!(e, CoreEvent::StatementFailed { .. }))
    {
        panic!("{sql}\n→ {error:?}");
    }
    events
}

/// The first cell of the last result of a script, as text.
async fn value(open: &OpenConnection, sql: &str) -> String {
    let events = script(open, sql).await;
    let result = events
        .iter()
        .rev()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .expect("a result");
    let stored = open.session.results().get(result).unwrap();
    let stored = stored.read().unwrap();
    stored
        .value_at(0, 0)
        .map_or_else(|| "NULL".into(), |v: Value| v.to_string())
}

/// A script as SQL*Plus reads it: statements end at `;` (which is not sent), PL/SQL blocks keep
/// theirs until a `/` line, and each runs on the server as one statement.
#[tokio::test]
async fn scripts_with_plsql_blocks_run_as_sqlplus_reads_them() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("oracle-scripts");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("ora").await;
    let table = format!("spelaion_script_{}", std::process::id());
    let events = script(
        &open,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (id number(9) primary key, note varchar2(40));\n\
             begin\n  for i in 1 .. 3 loop\n    insert into {table} values (i, q'[it's; row ]' || i);\n  end loop;\n  commit;\nend;\n/\n\
             select count(*) from {table}"
        ),
    )
    .await;
    let finished = events
        .iter()
        .filter(|e| matches!(e, CoreEvent::StatementFinished { .. }))
        .count();
    assert_eq!(finished, 4, "{events:?}");
    assert_eq!(
        value(&open, &format!("select count(*) from {table}")).await,
        "3"
    );
    assert_eq!(
        value(&open, &format!("select note from {table} where id = 2")).await,
        "it's; row 2"
    );
    script(&open, &format!("drop table {table} purge")).await;
    open.close().await;
}

/// The transaction state comes from the driver after every statement (ADR-031): an `INSERT`
/// opens one, `ROLLBACK` ends it, and closing then needs no confirmation.
#[tokio::test]
async fn the_transaction_is_read_from_the_driver() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("oracle-transactions");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(&host, port, ""),
    );
    let open = ws.open("ora").await;
    let table = format!("spelaion_tx_{}", std::process::id());
    script(
        &open,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (id number(9))"
        ),
    )
    .await;
    script(&open, &format!("insert into {table} values (1)")).await;
    assert_eq!(
        open.session.connection().transaction_state().await.unwrap(),
        TransactionState::Open
    );
    script(&open, "rollback").await;
    assert_eq!(
        open.session.connection().transaction_state().await.unwrap(),
        TransactionState::Idle
    );
    assert_eq!(
        value(&open, &format!("select count(*) from {table}")).await,
        "0"
    );
    script(&open, &format!("drop table {table} purge")).await;
    open.close().await;
}

/// ADR-059, point 2: read-only connections and agents wait for a barrier that stands up to
/// tests that try to get around it, so neither the configuration nor `[mcp]` turns them on.
#[test]
fn oracle_has_no_read_only_mode_nor_agents_yet() {
    let _turn = crate::serial::file_turn(module_path!());
    let ws = Workspace::new("oracle-no-barrier");
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection("db", 1521, "read_only = true\n"),
    );
    let problems: Vec<String> = ws.config().problems.iter().map(|p| p.to_string()).collect();
    assert!(
        problems
            .iter()
            .any(|p| p.contains("Oracle has no read-only mode")),
        "{problems:?}"
    );

    let ws = Workspace::new("oracle-no-agents");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection("db", 1521, "\n[mcp]\nexposed = true\nwrites = \"allow\"\n"),
    );
    let config = ws.config();
    assert!(config.problems.is_empty(), "{:?}", config.problems);
    for all_read_only in [false, true] {
        assert_eq!(
            agent_view(&config.connections[0], true, all_read_only),
            AgentView::EngineWithoutBarrier
        );
    }
}

/// The error of the first statement of `sql` that fails, if any.
async fn failure(open: &OpenConnection, sql: &str) -> Option<String> {
    let events: Vec<CoreEvent> = open.session.run_script(sql).collect().await;
    events.into_iter().find_map(|e| match e {
        CoreEvent::StatementFailed { error, .. } => Some(error.message),
        _ => None,
    })
}

/// Why Oracle still has no read barrier (ADR-059, the 2026-11-23 note; SDB-339). Each session
/// setting a restricted session could start with stops a function that writes behind a
/// `SELECT` in an autonomous transaction, and none stops one that undoes the setting first: an
/// `ALTER SESSION` inside `EXECUTE IMMEDIATE` needs no privilege and runs from inside a query.
/// - `SET TRANSACTION READ ONLY` does not reach the autonomous transaction at all.
/// - `ALTER SESSION DISABLE COMMIT IN PROCEDURE` refuses its `COMMIT` (ORA-00034), and a
///   function that runs `ALTER SESSION ENABLE COMMIT IN PROCEDURE` first commits anyway.
/// - `ALTER SESSION SET READ_ONLY = TRUE` (23ai) refuses its `INSERT` (ORA-28193), and a
///   function that runs `ALTER SESSION SET READ_ONLY = FALSE` first writes anyway.
///
/// If this test fails, Oracle has changed what these settings do: read the note again before
/// turning the barrier on.
#[tokio::test]
async fn no_session_setting_stops_a_function_that_undoes_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = Workspace::new("oracle-get-around");
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(&host, port, ""),
    );
    let owner = ws.open("ora").await;
    let id = std::process::id();
    let table = format!("spelaion_written_{id}");
    let plain = format!("spelaion_plain_{id}");
    let undo_commit = format!("spelaion_undo_commit_{id}");
    let undo_read_only = format!("spelaion_undo_read_only_{id}");
    // A function declared with an autonomous transaction, which writes a row and commits it;
    // `undo` runs before the write.
    let function = |name: &str, undo: &str| {
        format!(
            "create or replace function {name} return number is\n  pragma autonomous_transaction;\n\
             begin\n  {undo}\n  insert into {table} values ('{name}');\n  commit;\n  return 1;\nend;\n/\n"
        )
    };
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n{}{}{}",
            function(&plain, "null;"),
            function(
                &undo_commit,
                "execute immediate 'alter session enable commit in procedure';"
            ),
            function(
                &undo_read_only,
                "execute immediate 'alter session set read_only = false';"
            ),
        ),
    )
    .await;
    let written = |name: &str| format!("select count(*) from {table} where note = '{name}'");

    // A read-only transaction: the autonomous transaction writes anyway.
    let open = ws.open("ora").await;
    script(&open, "set transaction read only").await;
    assert_eq!(
        value(&open, &format!("select {plain}() from dual")).await,
        "1"
    );
    script(&open, "rollback").await;
    open.close().await;
    assert_eq!(value(&owner, &written(&plain)).await, "1");

    // Commits disabled in procedures: refused, until the function enables them again.
    let open = ws.open("ora").await;
    script(&open, "alter session disable commit in procedure").await;
    script(&open, "set transaction read only").await;
    let refused = failure(&open, &format!("select {plain}() from dual")).await;
    assert!(
        refused.as_deref().is_some_and(|e| e.contains("ORA-00034")),
        "{refused:?}"
    );
    script(&open, "rollback").await;
    script(&open, "set transaction read only").await;
    assert_eq!(
        value(&open, &format!("select {undo_commit}() from dual")).await,
        "1"
    );
    script(&open, "rollback").await;
    open.close().await;
    assert_eq!(value(&owner, &written(&plain)).await, "1");
    assert_eq!(value(&owner, &written(&undo_commit)).await, "1");

    // A read-only session: refused, until the function makes the session writable again.
    let open = ws.open("ora").await;
    script(&open, "alter session set read_only = true").await;
    let refused = failure(&open, &format!("select {plain}() from dual")).await;
    assert!(
        refused.as_deref().is_some_and(|e| e.contains("ORA-28193")),
        "{refused:?}"
    );
    assert_eq!(
        value(&open, &format!("select {undo_read_only}() from dual")).await,
        "1"
    );
    open.close().await;
    assert_eq!(value(&owner, &written(&plain)).await, "1");
    assert_eq!(value(&owner, &written(&undo_read_only)).await, "1");

    script(
        &owner,
        &format!(
            "drop function {plain};\ndrop function {undo_commit};\ndrop function {undo_read_only};\n\
             drop table {table} purge"
        ),
    )
    .await;
    owner.close().await;
}

// ADR-080: Oracle's read barrier is a database user the database itself keeps read-only.
// `testing/docker` makes one (`oracle/startup/02-spelaion-ro.sql`); the tests below are the
// ways around a session setting that SDB-339 tried by hand, run under that user, and the
// experiment that settles how a session proves it is read-only (ADR-080's note of 2026-11-28).
// Nothing here turns a barrier on: `oracle_has_no_read_only_mode_nor_agents_yet` still holds.

/// `testing/docker`'s read-only user, password `spelaion`, with SELECT on `spelaion_ro_reads`.
const READ_ONLY_USER: &str = "spelaion_ro";
/// `testing/docker`'s administrator (`system`, `ORACLE_PWD`), for what only a DBA does.
const DBA_PASSWORD: &str = "spelaion-test-Pw1";

/// A workspace with three connections to FREEPDB1: `ora` (the writable `spelaion`), `ro` (the
/// read-only user) and `dba` (`system`, opened with `DBA_PASSWORD`).
fn accounts(label: &str, host: &str, port: u16) -> Workspace {
    let ws = Workspace::new(label);
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(host, port, ""),
    );
    ws.write(
        ".spelaion/connections/ro.toml",
        &connection_as(host, port, READ_ONLY_USER, ""),
    );
    ws.write(
        ".spelaion/connections/dba.toml",
        &connection_as(host, port, "system", ""),
    );
    ws
}

/// The first statement of `sql` fails with ORA-28194, the read-only user's refusal.
async fn refused(open: &OpenConnection, sql: &str) {
    let error = failure(open, sql).await;
    assert!(
        error.as_deref().is_some_and(|e| e.contains("ORA-28194")),
        "{sql}\n→ {error:?}"
    );
}

/// Whether the session has a transaction, by the database's own answer.
async fn in_transaction(open: &OpenConnection) -> bool {
    value(
        open,
        "select case when dbms_transaction.local_transaction_id is null then 'no' else 'yes' end from dual",
    )
    .await
        == "yes"
}

/// The session's id on the server, for what the administrator asks about it.
async fn sid(open: &OpenConnection) -> String {
    value(open, "select sys_context('userenv', 'sid') from dual").await
}

/// How many transaction and table locks the server holds for session `sid`.
async fn locks(dba: &OpenConnection, sid: &str) -> String {
    value(
        dba,
        &format!("select count(*) from v$lock where sid = {sid} and type in ('TX', 'TM')"),
    )
    .await
}

/// Waits, up to a minute, until the first cell of `sql` reads `expected`.
async fn eventually(open: &OpenConnection, sql: &str, expected: &str) {
    let mut last = String::new();
    for _ in 0..120 {
        last = value(open, sql).await;
        if last == expected {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    panic!("{sql}\n→ {last}, not {expected}");
}

/// A function that writes `note` into `table` in an autonomous transaction and commits it,
/// called from a `SELECT`; `body` runs before the write.
fn writer(name: &str, header: &str, body: &str, write: &str) -> String {
    format!(
        "create or replace function {name} return number {header} is\n  pragma autonomous_transaction;\n\
         begin\n  {body}\n  {write}\n  commit;\n  return 1;\nend;\n/\n"
    )
}

/// ADR-080's barrier holds against what got around every session setting (SDB-339; ADR-059's
/// note of 2026-11-23). Under the read-only user each of these fails with ORA-28194 and writes
/// nothing: a write in an autonomous transaction with the session's own rights, the same with
/// the rights of the writable user that owns the function, functions that first undo
/// `DISABLE COMMIT IN PROCEDURE` and `READ_ONLY = TRUE`, a write through a database link,
/// directly and from such a function, and a scheduler job created from the query (it is
/// created, runs and fails with the same error). The same functions write under their owner.
#[tokio::test]
async fn the_read_only_user_refuses_every_way_around_that_writes() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-get-around", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let table = format!("spelaion_ro_written_{id}");
    let link = format!("spelaion_ro_link_{id}");
    let job = format!("SPELAION_RO_JOB_{id}");
    let own_rights = format!("spelaion_ro_own_rights_{id}");
    let definer = format!("spelaion_ro_definer_{id}");
    let undo_commit = format!("spelaion_ro_undo_commit_{id}");
    let undo_read_only = format!("spelaion_ro_undo_read_only_{id}");
    let through_link = format!("spelaion_ro_through_link_{id}");
    let scheduler = format!("spelaion_ro_job_{id}");
    let insert = |note: &str| format!("insert into spelaion.{table} values ('{note}');");
    script(
        &dba,
        &format!(
            "begin execute immediate 'drop public database link {link}'; exception when others then null; end;\n/\n\
             create public database link {link} connect to spelaion identified by spelaion using '//localhost:1521/FREEPDB1'"
        ),
    )
    .await;
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n\
             grant select, insert, update, delete on {table} to {READ_ONLY_USER};\n\
             {}{}{}{}{}\
             create or replace function {scheduler}(job_name varchar2) return number is\n  pragma autonomous_transaction;\n\
             begin\n  dbms_scheduler.create_job(job_name => job_name, job_type => 'PLSQL_BLOCK', \
             job_action => q'[begin {} commit; end;]', enabled => true);\n  commit;\n  return 1;\nend;\n/\n\
             grant execute on {own_rights} to {READ_ONLY_USER};\n\
             grant execute on {definer} to {READ_ONLY_USER};\n\
             grant execute on {undo_commit} to {READ_ONLY_USER};\n\
             grant execute on {undo_read_only} to {READ_ONLY_USER};\n\
             grant execute on {through_link} to {READ_ONLY_USER};\n\
             grant execute on {scheduler} to {READ_ONLY_USER}",
            writer(&own_rights, "authid current_user", "null;", &insert(&own_rights)),
            writer(&definer, "", "null;", &insert(&definer)),
            writer(
                &undo_commit,
                "",
                "execute immediate 'alter session enable commit in procedure';",
                &insert(&undo_commit)
            ),
            writer(
                &undo_read_only,
                "",
                "execute immediate 'alter session set read_only = false';",
                &insert(&undo_read_only)
            ),
            writer(
                &through_link,
                "",
                "null;",
                &format!("insert into spelaion.{table}@{link} values ('{through_link}');")
            ),
            insert(&scheduler),
        ),
    )
    .await;
    let written = |note: &str| format!("select count(*) from {table} where note = '{note}'");
    let functions = [
        &own_rights,
        &definer,
        &undo_commit,
        &undo_read_only,
        &through_link,
    ];

    // Each attempt in a session of its own, after `setting` (a link leaves a transaction open,
    // and a session setting cannot change while one is).
    let attempt = async |setting: &str, sql: &str| {
        let ro = ws.open("ro").await;
        if !setting.is_empty() {
            script(&ro, setting).await;
        }
        refused(&ro, sql).await;
        ro.close().await;
    };
    attempt(
        "",
        &format!("insert into spelaion.{table} values ('direct')"),
    )
    .await;
    attempt(
        "",
        &format!("insert into spelaion.{table}@{link} values ('direct_link')"),
    )
    .await;
    for function in functions {
        attempt("", &format!("select spelaion.{function}() from dual")).await;
    }
    // A restricted session's settings, undone by the function: the user still refuses.
    attempt(
        "alter session disable commit in procedure",
        &format!("select spelaion.{undo_commit}() from dual"),
    )
    .await;
    attempt(
        "alter session set read_only = true",
        &format!("select spelaion.{undo_read_only}() from dual"),
    )
    .await;
    attempt(
        "",
        &format!("select spelaion.{scheduler}('{job}_RO') from dual"),
    )
    .await;
    // The job was created anyway, and ran: its own write was refused too.
    eventually(
        &owner,
        &format!(
            "select max(status) from user_scheduler_job_run_details where job_name = '{job}_RO'"
        ),
        "FAILED",
    )
    .await;
    assert_eq!(
        value(&owner, &format!("select count(*) from {table}")).await,
        "0"
    );

    // Under the writable user that owns them, the same functions write.
    for function in functions {
        assert_eq!(
            value(&owner, &format!("select {function}() from dual")).await,
            "1"
        );
        assert_eq!(value(&owner, &written(function)).await, "1", "{function}");
    }
    assert_eq!(
        value(&owner, &format!("select {scheduler}('{job}_RW') from dual")).await,
        "1"
    );
    eventually(&owner, &written(&scheduler), "1").await;

    script(
        &owner,
        &format!(
            "drop function {own_rights};\ndrop function {definer};\ndrop function {undo_commit};\n\
             drop function {undo_read_only};\ndrop function {through_link};\ndrop function {scheduler};\n\
             drop table {table} purge"
        ),
    )
    .await;
    owner.close().await;
    script(&dba, &format!("drop public database link {link}")).await;
    dba.close().await;
}

/// ADR-080, point 3, the dictionary's answer: `SYS.USER_USERS.READ_ONLY` says whether the
/// session's own user is read-only (`YES` / `NO`), and needs no privilege. It is asked by the
/// view's full name because the user's own schema can answer for the short one. On a release
/// before 23ai the column does not exist and the question fails with ORA-00904, as it does here
/// for a column this release lacks: a failed question is a no (point 4).
#[tokio::test]
async fn the_dictionary_says_whether_the_sessions_user_is_read_only() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-dictionary", &host, port);
    let question = "select read_only from sys.user_users";
    let ro = ws.open("ro").await;
    assert_eq!(value(&ro, question).await, "YES");
    assert_eq!(
        value(
            &ro,
            "select note from spelaion.spelaion_ro_reads where id = 1"
        )
        .await,
        "read"
    );
    ro.close().await;

    let owner = ws.open("ora").await;
    assert_eq!(value(&owner, question).await, "NO");
    script(
        &owner,
        "create or replace view user_users as select 'YES' read_only from dual",
    )
    .await;
    assert_eq!(
        value(&owner, "select read_only from user_users").await,
        "YES"
    );
    assert_eq!(value(&owner, question).await, "NO");
    script(&owner, "drop view user_users").await;
    let missing = failure(&owner, "select spelaion_no_such_column from sys.user_users").await;
    assert!(
        missing.as_deref().is_some_and(|e| e.contains("ORA-00904")),
        "{missing:?}"
    );
    owner.close().await;
}

/// ADR-080, point 3, the session's answer: the write the check tries is `ROLLBACK`, right after
/// connecting. A read-only session refuses it with ORA-28194 (and `COMMIT` and `SAVEPOINT`,
/// even with no transaction) and is left without one. A writable session that has just
/// connected has nothing to roll back: it changes nothing, opens no transaction and holds no
/// lock. The other writes a read-only session refuses do leave a writable one holding locks in
/// a transaction: a `SAVEPOINT`, and `SELECT … FOR UPDATE` even when it selects no row.
#[tokio::test]
async fn rollback_is_the_write_a_read_only_session_refuses_and_a_writable_one_ignores() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-probe", &host, port);
    let dba = ws.open_with("dba", DBA_PASSWORD).await;

    let ro = ws.open("ro").await;
    for write in ["rollback", "commit", "savepoint spelaion_probe"] {
        refused(&ro, write).await;
        assert!(!in_transaction(&ro).await, "{write}");
    }
    let ro_sid = sid(&ro).await;
    assert_eq!(locks(&dba, &ro_sid).await, "0");
    assert_eq!(
        ro.session.connection().transaction_state().await.unwrap(),
        TransactionState::Idle
    );
    ro.close().await;

    let owner = ws.open("ora").await;
    let owner_sid = sid(&owner).await;
    script(&owner, "rollback").await;
    assert!(!in_transaction(&owner).await);
    assert_eq!(locks(&dba, &owner_sid).await, "0");
    assert_eq!(
        owner
            .session
            .connection()
            .transaction_state()
            .await
            .unwrap(),
        TransactionState::Idle
    );
    for write in [
        "savepoint spelaion_probe",
        "select dummy from sys.dual where 1 = 0 for update",
    ] {
        script(&owner, write).await;
        assert!(in_transaction(&owner).await, "{write}");
        assert_ne!(locks(&dba, &owner_sid).await, "0", "{write}");
        script(&owner, "rollback").await;
    }
    owner.close().await;
    dba.close().await;
}

/// ADR-080, point 7: a statement the read-only user refuses leaves no transaction, and the
/// `ROLLBACK` after it is refused all the same. A read through a database link does open a
/// transaction, which neither `ROLLBACK` nor `COMMIT` can end in a read-only session: the
/// session still reads, and closing it is what ends the transaction.
#[tokio::test]
async fn a_refused_rollback_ends_nothing_and_closing_the_session_does() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-rollback", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let table = format!("spelaion_ro_ending_{id}");
    let link = format!("spelaion_ro_ending_link_{id}");
    script(
        &dba,
        &format!(
            "begin execute immediate 'drop public database link {link}'; exception when others then null; end;\n/\n\
             create public database link {link} connect to spelaion identified by spelaion using '//localhost:1521/FREEPDB1'"
        ),
    )
    .await;
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n\
             grant select, insert on {table} to {READ_ONLY_USER}"
        ),
    )
    .await;

    let ro = ws.open("ro").await;
    refused(
        &ro,
        &format!("insert into spelaion.{table} values ('refused')"),
    )
    .await;
    assert!(!in_transaction(&ro).await);
    refused(&ro, "rollback").await;
    assert!(!in_transaction(&ro).await);
    assert_eq!(
        ro.session.connection().transaction_state().await.unwrap(),
        TransactionState::Idle
    );

    assert_eq!(
        value(
            &ro,
            &format!("select count(*) from spelaion.{table}@{link}")
        )
        .await,
        "0"
    );
    assert!(in_transaction(&ro).await);
    refused(&ro, "rollback").await;
    refused(&ro, "commit").await;
    assert!(in_transaction(&ro).await);
    assert_eq!(
        ro.session.connection().transaction_state().await.unwrap(),
        TransactionState::Open
    );
    assert_eq!(
        value(
            &ro,
            "select note from spelaion.spelaion_ro_reads where id = 1"
        )
        .await,
        "read"
    );
    let ro_sid = sid(&ro).await;
    let serial = value(
        &dba,
        &format!("select serial# from v$session where sid = {ro_sid}"),
    )
    .await;
    ro.close().await;
    eventually(
        &dba,
        &format!("select count(*) from v$session where sid = {ro_sid} and serial# = {serial}"),
        "0",
    )
    .await;

    script(&owner, &format!("drop table {table} purge")).await;
    owner.close().await;
    script(&dba, &format!("drop public database link {link}")).await;
    dba.close().await;
}

/// ADR-080, point 3: a session keeps what its user was when it connected, and the dictionary
/// says what the user is now. A session that connected before `ALTER USER … READ ONLY` is told
/// `YES` by the dictionary and still writes, and its `ROLLBACK` goes through; one that
/// connected before `ALTER USER … READ WRITE` is told `NO` and still refuses. Each fails one of
/// the two answers, which is why the check needs both.
#[tokio::test]
async fn a_session_keeps_what_its_user_was_when_it_connected() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-switch", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let user = format!("spelaion_switch_{id}");
    let table = format!("spelaion_switch_written_{id}");
    ws.write(
        ".spelaion/connections/switch.toml",
        &connection_as(&host, port, &user, ""),
    );
    script(
        &dba,
        &format!(
            "begin execute immediate 'drop user {user} cascade'; exception when others then null; end;\n/\n\
             create user {user} identified by spelaion;\n\
             grant create session to {user}"
        ),
    )
    .await;
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n\
             grant select, insert on {table} to {user}"
        ),
    )
    .await;
    let question = "select read_only from sys.user_users";

    let before_read_only = ws.open("switch").await;
    assert_eq!(value(&before_read_only, question).await, "NO");
    script(&dba, &format!("alter user {user} read only")).await;
    assert_eq!(value(&before_read_only, question).await, "YES");
    script(&before_read_only, "rollback").await;
    script(
        &before_read_only,
        &format!("insert into spelaion.{table} values ('before read only');\ncommit"),
    )
    .await;
    assert_eq!(
        value(&owner, &format!("select count(*) from {table}")).await,
        "1"
    );
    before_read_only.close().await;

    let before_read_write = ws.open("switch").await;
    assert_eq!(value(&before_read_write, question).await, "YES");
    refused(&before_read_write, "rollback").await;
    script(&dba, &format!("alter user {user} read write")).await;
    assert_eq!(value(&before_read_write, question).await, "NO");
    refused(&before_read_write, "rollback").await;
    refused(
        &before_read_write,
        &format!("insert into spelaion.{table} values ('before read write')"),
    )
    .await;
    before_read_write.close().await;
    assert_eq!(
        value(&owner, &format!("select count(*) from {table}")).await,
        "1"
    );

    script(&owner, &format!("drop table {table} purge")).await;
    owner.close().await;
    eventually(
        &dba,
        &format!("select count(*) from v$session where username = upper('{user}')"),
        "0",
    )
    .await;
    script(&dba, &format!("drop user {user} cascade")).await;
    dba.close().await;
}

/// ADR-080, point 8, and what contradicts it (the note of 2026-11-28): a database link that
/// connects as a writable user lets a read-only session write, in this same database. Writing
/// to a table through the link is refused (ORA-28194), directly and hidden in a view or a
/// synonym; but calling the writable user's function through the link runs it in the link's
/// session, which is not read-only, and a function with an autonomous transaction commits its
/// write there: called directly, hidden in a view the read-only user may read, or behind a
/// synonym. The statement names no link in the last two. If this test fails, Oracle has started
/// refusing that call: read the note again.
#[tokio::test]
async fn a_link_to_a_writable_user_lets_a_remote_function_write() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-link", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let table = format!("spelaion_ro_remote_{id}");
    let link = format!("spelaion_ro_remote_link_{id}");
    let remote = format!("spelaion_ro_remote_write_{id}");
    let rows_view = format!("spelaion_ro_rows_view_{id}");
    let rows_synonym = format!("spelaion_ro_rows_syn_{id}");
    let call_view = format!("spelaion_ro_call_view_{id}");
    let call_synonym = format!("spelaion_ro_call_syn_{id}");
    script(
        &dba,
        &format!(
            "begin execute immediate 'drop public database link {link}'; exception when others then null; end;\n/\n\
             create public database link {link} connect to spelaion identified by spelaion using '//localhost:1521/FREEPDB1'"
        ),
    )
    .await;
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n\
             create or replace function {remote}(note varchar2) return number is\n  pragma autonomous_transaction;\n\
             begin\n  insert into {table} values (note);\n  commit;\n  return 1;\nend;\n/\n\
             create or replace view {rows_view} as select note from spelaion.{table}@{link};\n\
             create or replace synonym {rows_synonym} for spelaion.{table}@{link};\n\
             create or replace view {call_view} as select spelaion.{remote}@{link}('view') written from dual;\n\
             create or replace synonym {call_synonym} for spelaion.{remote}@{link};\n\
             grant select, insert on {rows_view} to {READ_ONLY_USER};\n\
             grant select on {call_view} to {READ_ONLY_USER}"
        ),
    )
    .await;
    let written = |note: &str| format!("select count(*) from {table} where note = '{note}'");

    let ro = ws.open("ro").await;
    refused(
        &ro,
        &format!("insert into spelaion.{table}@{link} values ('rows direct')"),
    )
    .await;
    refused(
        &ro,
        &format!("insert into spelaion.{rows_view} values ('rows view')"),
    )
    .await;
    refused(
        &ro,
        &format!("insert into spelaion.{rows_synonym} values ('rows synonym')"),
    )
    .await;
    assert_eq!(
        value(&ro, &format!("select count(*) from spelaion.{rows_view}")).await,
        "0"
    );
    assert_eq!(
        value(
            &ro,
            &format!("select spelaion.{remote}@{link}('direct') from dual")
        )
        .await,
        "1"
    );
    assert_eq!(
        value(&ro, &format!("select written from spelaion.{call_view}")).await,
        "1"
    );
    assert_eq!(
        value(
            &ro,
            &format!("select spelaion.{call_synonym}('synonym') from dual")
        )
        .await,
        "1"
    );
    ro.close().await;
    for note in ["rows direct", "rows view", "rows synonym"] {
        assert_eq!(value(&owner, &written(note)).await, "0", "{note}");
    }
    for note in ["direct", "view", "synonym"] {
        assert_eq!(value(&owner, &written(note)).await, "1", "{note}");
    }

    script(
        &owner,
        &format!(
            "drop synonym {call_synonym};\ndrop view {call_view};\ndrop synonym {rows_synonym};\n\
             drop view {rows_view};\ndrop function {remote};\ndrop table {table} purge"
        ),
    )
    .await;
    owner.close().await;
    script(&dba, &format!("drop public database link {link}")).await;
    dba.close().await;
}

/// Which links a read-only session reaches, and which it can see (ADR-092). A writable user's
/// private link does not resolve from the read-only session's own statements, even hidden in
/// that user's view or synonym (ORA-04054). But that user's definer's-rights code resolves it
/// in its owner's schema and writes through it: a function that calls a remote function with
/// an autonomous transaction, a procedure that calls a remote procedure that commits (no
/// autonomous transaction needed), and a function that names the link only inside
/// `EXECUTE IMMEDIATE`, which leaves no trace in `ALL_DEPENDENCIES`. The session's
/// `ALL_DB_LINKS` does not show that link; `DBA_DB_LINKS` does, and the read-only user may
/// read it only if a DBA allows it (here with `SELECT_CATALOG_ROLE`). A remote procedure
/// called from a PL/SQL block through a public link commits too, and a remote function
/// without an autonomous transaction is refused inside a query (ORA-14551).
#[tokio::test]
async fn a_writable_users_code_reaches_its_private_link_where_the_session_cannot_see_it() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-private-link", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let table = format!("spelaion_ro_reach_{id}");
    let private = format!("spelaion_ro_private_{id}");
    let public = format!("spelaion_ro_public_{id}");
    let remote = format!("spelaion_ro_reach_write_{id}");
    let plain = format!("spelaion_ro_reach_plain_{id}");
    let procedure = format!("spelaion_ro_reach_proc_{id}");
    let definer = format!("spelaion_ro_reach_definer_{id}");
    let definer_procedure = format!("spelaion_ro_reach_definer_proc_{id}");
    let dynamic = format!("spelaion_ro_reach_dynamic_{id}");
    let view = format!("spelaion_ro_reach_view_{id}");
    let synonym = format!("spelaion_ro_reach_syn_{id}");
    let checker = format!("spelaion_ro_checker_{id}");
    let address = "'//localhost:1521/FREEPDB1'";
    script(
        &dba,
        &format!(
            "begin execute immediate 'drop public database link {public}'; exception when others then null; end;\n/\n\
             begin execute immediate 'drop user {checker} cascade'; exception when others then null; end;\n/\n\
             create public database link {public} connect to spelaion identified by spelaion using {address};\n\
             create user {checker} identified by spelaion;\n\
             grant create session, select_catalog_role to {checker};\n\
             alter user {checker} read only"
        ),
    )
    .await;
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             begin execute immediate 'drop database link {private}'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n\
             create database link {private} connect to spelaion identified by spelaion using {address};\n\
             create or replace function {remote}(note varchar2) return number is\n  pragma autonomous_transaction;\n\
             begin\n  insert into {table} values (note);\n  commit;\n  return 1;\nend;\n/\n\
             create or replace function {plain}(note varchar2) return number is\n\
             begin\n  insert into {table} values (note);\n  return 1;\nend;\n/\n\
             create or replace procedure {procedure}(note varchar2) is\n\
             begin\n  insert into {table} values (note);\n  commit;\nend;\n/\n\
             create or replace function {definer}(note varchar2) return number is\n\
             begin\n  return {remote}@{private}(note);\nend;\n/\n\
             create or replace procedure {definer_procedure}(note varchar2) is\n\
             begin\n  {procedure}@{private}(note);\nend;\n/\n\
             create or replace function {dynamic}(note varchar2) return number is\n  written number;\n\
             begin\n  execute immediate 'select {remote}@{private}(:1) from dual' into written using note;\n  return written;\nend;\n/\n\
             create or replace view {view} as select spelaion.{remote}@{private}('view') written from dual;\n\
             create or replace synonym {synonym} for spelaion.{remote}@{private};\n\
             grant select on {view} to {READ_ONLY_USER};\n\
             grant execute on {definer} to {READ_ONLY_USER};\n\
             grant execute on {definer_procedure} to {READ_ONLY_USER};\n\
             grant execute on {dynamic} to {READ_ONLY_USER}"
        ),
    )
    .await;
    let written = |note: &str| format!("select count(*) from {table} where note = '{note}'");
    let link_named = |view: &str, name: &str| {
        format!("select count(*) from sys.{view} where db_link like upper('{name}') || '%'")
    };

    let ro = ws.open("ro").await;
    // What the session sees: the public link, not the writable user's private one.
    assert_eq!(value(&ro, &link_named("all_db_links", &public)).await, "1");
    assert_eq!(value(&ro, &link_named("all_db_links", &private)).await, "0");
    let hidden = failure(&ro, "select count(*) from sys.dba_db_links").await;
    assert!(
        hidden.as_deref().is_some_and(|e| e.contains("ORA-00942")),
        "{hidden:?}"
    );
    // The private link does not resolve from the session's own statements.
    for sql in [
        format!("select written from spelaion.{view}"),
        format!("select spelaion.{synonym}('synonym') from dual"),
    ] {
        let error = failure(&ro, &sql).await;
        assert!(
            error.as_deref().is_some_and(|e| e.contains("ORA-04054")),
            "{sql}\n→ {error:?}"
        );
    }
    // The writable user's own code resolves it, and writes.
    assert_eq!(
        value(
            &ro,
            &format!("select spelaion.{definer}('definer function') from dual")
        )
        .await,
        "1"
    );
    script(
        &ro,
        &format!("begin\n  spelaion.{definer_procedure}('definer procedure');\nend;\n/"),
    )
    .await;
    assert_eq!(
        value(
            &ro,
            &format!("select spelaion.{dynamic}('dynamic') from dual")
        )
        .await,
        "1"
    );
    // Only the static call is in the dictionary's dependencies.
    let depends = |name: &str| {
        format!(
            "select count(*) from sys.all_dependencies where owner = 'SPELAION' \
             and name = upper('{name}') and referenced_link_name like upper('{private}') || '%'"
        )
    };
    assert_eq!(value(&ro, &depends(&definer)).await, "1");
    assert_eq!(value(&ro, &depends(&dynamic)).await, "0");
    ro.close().await;

    // Through a public link, from the session's own block: a remote procedure commits.
    let ro = ws.open("ro").await;
    script(
        &ro,
        &format!("begin\n  spelaion.{procedure}@{public}('public procedure');\nend;\n/"),
    )
    .await;
    ro.close().await;
    let ro = ws.open("ro").await;
    let refused_in_query = failure(
        &ro,
        &format!("select spelaion.{plain}@{public}('public plain') from dual"),
    )
    .await;
    assert!(
        refused_in_query
            .as_deref()
            .is_some_and(|e| e.contains("ORA-14551")),
        "{refused_in_query:?}"
    );
    ro.close().await;

    for note in [
        "definer function",
        "definer procedure",
        "dynamic",
        "public procedure",
    ] {
        assert_eq!(value(&owner, &written(note)).await, "1", "{note}");
    }
    for note in ["view", "synonym", "public plain"] {
        assert_eq!(value(&owner, &written(note)).await, "0", "{note}");
    }

    // A read-only user a DBA lets read the dictionary sees every link of the database.
    ws.write(
        ".spelaion/connections/checker.toml",
        &connection_as(&host, port, &checker, ""),
    );
    let check = ws.open("checker").await;
    assert_eq!(
        value(&check, "select read_only from sys.user_users").await,
        "YES"
    );
    assert_eq!(
        value(&check, &link_named("dba_db_links", &private)).await,
        "1"
    );
    assert_eq!(
        value(&check, &link_named("dba_db_links", &public)).await,
        "1"
    );
    check.close().await;

    script(
        &owner,
        &format!(
            "drop synonym {synonym};\ndrop view {view};\ndrop function {dynamic};\n\
             drop procedure {definer_procedure};\ndrop function {definer};\ndrop procedure {procedure};\n\
             drop function {plain};\ndrop function {remote};\ndrop database link {private};\n\
             drop table {table} purge"
        ),
    )
    .await;
    owner.close().await;
    eventually(
        &dba,
        &format!("select count(*) from v$session where username = upper('{checker}')"),
        "0",
    )
    .await;
    script(
        &dba,
        &format!("drop public database link {public};\ndrop user {checker} cascade"),
    )
    .await;
    dba.close().await;
}

/// `SET TRANSACTION READ ONLY` in a read-only session (ADR-092, against ADR-080's point 6):
/// the database allows it, and it opens a transaction that the session can then never end,
/// since its `ROLLBACK` and `COMMIT` are refused. Every later read sees the data as it was
/// when that transaction began. Without it, a read leaves no transaction, and the driver
/// reports the session idle: there is nothing to end.
#[tokio::test]
async fn set_transaction_read_only_traps_a_read_only_session_in_its_snapshot() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-snapshot", &host, port);
    let owner = ws.open("ora").await;
    let table = format!("spelaion_ro_snapshot_{}", std::process::id());
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (n number);\n\
             insert into {table} values (1);\n\
             commit;\n\
             grant select on {table} to {READ_ONLY_USER}"
        ),
    )
    .await;
    let count = format!("select count(*) from spelaion.{table}");

    let plain = ws.open("ro").await;
    assert_eq!(value(&plain, &count).await, "1");
    assert!(!in_transaction(&plain).await);
    assert_eq!(
        plain
            .session
            .connection()
            .transaction_state()
            .await
            .unwrap(),
        TransactionState::Idle
    );

    // A read-only transaction that begins within seconds of the table's DDL cannot read it
    // (ORA-01466), and could never begin again: give the snapshot a table older than itself.
    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    let trapped = ws.open("ro").await;
    script(&trapped, "set transaction read only").await;
    assert_eq!(value(&trapped, &count).await, "1");
    assert!(in_transaction(&trapped).await);
    script(&owner, &format!("insert into {table} values (2);\ncommit")).await;
    assert_eq!(value(&trapped, &count).await, "1");
    refused(&trapped, "rollback").await;
    refused(&trapped, "commit").await;
    assert!(in_transaction(&trapped).await);
    assert_eq!(value(&trapped, &count).await, "1");
    assert_eq!(value(&plain, &count).await, "2");
    trapped.close().await;
    plain.close().await;

    script(&owner, &format!("drop table {table} purge")).await;
    owner.close().await;
}

/// Through the two bastions of `testing/docker`, with TLS: the plugin connects to the tunnel's
/// local end (127.0.0.1) and checks the certificate against the database's name behind the
/// bastions, `oracle-tls`, signed by the test authority (ADR-059, ADR-041: a plugin opens no
/// tunnel).
#[tokio::test]
async fn oracle_through_two_bastions_with_tls_verified_by_name() {
    let _turn = crate::serial::file_turn(module_path!());
    if server().is_none() {
        return;
    }
    let Ok(addr) = std::env::var("SPELAION_TEST_SSH") else {
        eprintln!("SPELAION_TEST_SSH not set: test skipped");
        return;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let ws = Workspace::new("oracle-tunnel");
    let identity = toml_path(&keys().join("id_ed25519"));
    ws.write(
        ".spelaion/tunnels/bastions.toml",
        &format!(
            "schema_version = 1\n\n[[hops]]\nhost = \"{host}\"\nport = {port}\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n\n[[hops]]\nhost = \"bastion2\"\nuser = \"spelaion\"\nauth = [\"key\"]\nidentity_file = {identity}\n"
        ),
    );
    let ca = toml_path(&keys().join("ca.crt"));
    ws.write(
        ".spelaion/connections/ora.toml",
        &connection(
            "oracle-tls",
            2484,
            &format!("tunnel = \"bastions\"\n\n[tls]\nmode = \"verify-full\"\nca = {ca}\n"),
        ),
    );
    let open = ws.open("ora").await;
    assert_eq!(value(&open, "select user from dual").await, "SPELAION");
    assert_eq!(
        value(
            &open,
            "select count(*) from dual connect by level <= 100000"
        )
        .await,
        "100000"
    );
    let schemas = open.session.connection().list_schemas().await.unwrap();
    assert!(schemas.iter().any(|s| s.name == "SPELAION"));
    open.close().await;
}

/// One of the channels the decision on Oracle's read barrier left open (SDB-476): Java. A
/// writable user's Java stored function that opens a new JDBC connection back into this
/// database, as that user, writes under the read-only user: the new session is not read-only.
/// It needs only that Java's policy lets the function's owner open the socket
/// (`java.net.SocketPermission`, granted by a DBA, which `DBA_JAVA_POLICY` shows); network
/// access control entries play no part. Without that grant Java's policy refuses the call.
/// Oracle's default policy grants every socket to the roles `JAVAUSERPRIV`, `JAVASYSPRIV`,
/// `EJBCLIENT` and `JMXSERVER`, so a user given one of them can do the same.
///
/// The other two channels cannot run in `testing/docker`'s lite image, so they are not tested
/// here: `UTL_HTTP` and `UTL_TCP` fail every network access check (ORA-29332, because
/// `SYS.DBMS_NETWORK_ACL_ADMIN` is missing from the root), and external procedures cannot start
/// their agent (ORA-28575, because `libagtsh.so` and `$ORACLE_HOME/hs` are missing).
#[tokio::test]
async fn a_writable_users_java_writes_under_the_read_only_user_once_it_may_open_a_socket() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-java", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let table = format!("spelaion_ro_java_{id}");
    let class = format!("SpelaionRoJava{id}");
    let function = format!("spelaion_ro_java_write_{id}");
    // The socket permissions granted to `spelaion`, removed before and after the test.
    let forget_permissions = "begin\n  for p in (select seq from dba_java_policy where grantee = 'SPELAION') loop\n    \
         dbms_java.disable_permission(p.seq);\n    dbms_java.delete_permission(p.seq);\n  end loop;\nend;\n/";
    let socket_grants = "select count(*) from dba_java_policy where grantee = 'SPELAION' \
         and type_name = 'java.net.SocketPermission' and kind = 'GRANT' and enabled = 'ENABLED'";
    script(&dba, forget_permissions).await;
    script(
        &owner,
        &format!(
            "begin execute immediate 'drop table {table} purge'; exception when others then null; end;\n/\n\
             create table {table} (note varchar2(60));\n\
             begin\n  execute immediate q'[create or replace and compile java source named \"{class}\" as\n\
             import java.sql.*;\n\
             public class {class} {{\n\
             \x20 public static int write(String note) throws Exception {{\n\
             \x20   Connection c = DriverManager.getConnection(\"jdbc:oracle:thin:@//localhost:1521/FREEPDB1\", \"spelaion\", \"spelaion\");\n\
             \x20   try {{\n\
             \x20     c.setAutoCommit(false);\n\
             \x20     PreparedStatement s = c.prepareStatement(\"insert into spelaion.{table} values (?)\");\n\
             \x20     s.setString(1, note);\n\
             \x20     s.executeUpdate();\n\
             \x20     c.commit();\n\
             \x20     return 1;\n\
             \x20   }} finally {{\n\
             \x20     c.close();\n\
             \x20   }}\n\
             \x20 }}\n\
             }}]';\nend;\n/\n\
             create or replace function {function}(note varchar2) return number\n\
             as language java name '{class}.write(java.lang.String) return int';\n/\n\
             grant execute on {function} to {READ_ONLY_USER}"
        ),
    )
    .await;
    let call = |note: &str| format!("select spelaion.{function}('{note}') from dual");
    let written = |note: &str| format!("select count(*) from {table} where note = '{note}'");

    // Without the permission, Java's policy refuses the socket.
    assert_eq!(value(&dba, socket_grants).await, "0");
    let ro = ws.open("ro").await;
    let refused_socket = failure(&ro, &call("no permission")).await;
    assert!(
        refused_socket
            .as_deref()
            .is_some_and(|e| e.contains("java.net.SocketPermission")),
        "{refused_socket:?}"
    );
    ro.close().await;

    // With it, the read-only session's call writes through a session of its own.
    script(
        &dba,
        "begin\n  dbms_java.grant_permission('SPELAION', 'SYS:java.net.SocketPermission', \
         'localhost:1521', 'connect,resolve');\nend;\n/",
    )
    .await;
    assert_eq!(value(&dba, socket_grants).await, "1");
    let ro = ws.open("ro").await;
    assert_eq!(
        value(&ro, "select read_only from sys.user_users").await,
        "YES"
    );
    assert_eq!(value(&ro, &call("read-only")).await, "1");
    ro.close().await;
    assert_eq!(value(&owner, &written("read-only")).await, "1");
    assert_eq!(value(&owner, &written("no permission")).await, "0");

    // Oracle's default policy: any socket for these roles.
    assert_eq!(
        value(
            &dba,
            "select count(distinct grantee) from dba_java_policy \
             where type_name = 'java.net.SocketPermission' and kind = 'GRANT' \
             and enabled = 'ENABLED' and name = '*' \
             and grantee in ('JAVAUSERPRIV', 'JAVASYSPRIV', 'EJBCLIENT', 'JMXSERVER')"
        )
        .await,
        "4"
    );

    script(&dba, forget_permissions).await;
    script(
        &owner,
        &format!(
            "drop function {function};\ndrop java source \"{class}\";\ndrop table {table} purge"
        ),
    )
    .await;
    owner.close().await;
    dba.close().await;
}

/// The Java answer of ADR-092's note of 2026-11-28: how many Java objects (every `JAVA%`
/// object type) belong to an owner that is not one of Oracle's own (`ORACLE_MAINTAINED = 'Y'`),
/// asked by full `SYS` names. An owner missing from `DBA_USERS` counts as not Oracle's.
const JAVA_OUTSIDE_ORACLE: &str = "select count(*) from sys.dba_objects o \
     where o.object_type like 'JAVA%' and not exists (select 1 from sys.dba_users u \
     where u.username = o.owner and u.oracle_maintained = 'Y')";

/// ADR-092's note of 2026-11-28: read-only mode and agents on Oracle are refused while any Java
/// object exists outside Oracle's own schemas. A read-only user that a DBA lets read the
/// dictionary (here with `SELECT_CATALOG_ROLE`) asks it: none in `testing/docker`'s clean
/// database, Oracle's own tens of thousands of classes included; the writable user's Java
/// source, its class and the function over it, once that user loads them; none again once they
/// are dropped.
#[tokio::test]
async fn the_java_answer_finds_a_users_java_and_none_on_a_clean_database() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some((host, port)) = server() else { return };
    let ws = accounts("oracle-ro-java-answer", &host, port);
    let owner = ws.open("ora").await;
    let dba = ws.open_with("dba", DBA_PASSWORD).await;
    let id = std::process::id();
    let class = format!("SpelaionRoJavaAnswer{id}");
    let function = format!("spelaion_ro_java_answer_{id}");
    let checker = format!("spelaion_ro_java_checker_{id}");
    script(
        &dba,
        &format!(
            "begin execute immediate 'drop user {checker} cascade'; exception when others then null; end;\n/\n\
             create user {checker} identified by spelaion;\n\
             grant create session, select_catalog_role to {checker};\n\
             alter user {checker} read only"
        ),
    )
    .await;
    ws.write(
        ".spelaion/connections/checker.toml",
        &connection_as(&host, port, &checker, ""),
    );
    let mine = format!("{JAVA_OUTSIDE_ORACLE} and o.owner = 'SPELAION'");

    let check = ws.open("checker").await;
    assert_eq!(
        value(&check, "select read_only from sys.user_users").await,
        "YES"
    );
    assert_eq!(value(&check, JAVA_OUTSIDE_ORACLE).await, "0");
    assert_ne!(
        value(
            &check,
            "select count(*) from sys.dba_objects where object_type like 'JAVA%'"
        )
        .await,
        "0"
    );

    script(
        &owner,
        &format!(
            "begin\n  execute immediate q'[create or replace and compile java source named \"{class}\" as\n\
             public class {class} {{\n\
             \x20 public static int one() {{\n\
             \x20   return 1;\n\
             \x20 }}\n\
             }}]';\nend;\n/\n\
             create or replace function {function} return number\n\
             as language java name '{class}.one() return int';\n/"
        ),
    )
    .await;
    assert_eq!(
        value(&owner, &format!("select {function}() from dual")).await,
        "1"
    );
    // The source and its class are Java objects; the PL/SQL function over them is not.
    assert_eq!(value(&check, &mine).await, "2");
    assert_eq!(value(&check, JAVA_OUTSIDE_ORACLE).await, "2");

    script(
        &owner,
        &format!("drop function {function};\ndrop java source \"{class}\""),
    )
    .await;
    assert_eq!(value(&check, JAVA_OUTSIDE_ORACLE).await, "0");
    check.close().await;
    owner.close().await;
    eventually(
        &dba,
        &format!("select count(*) from v$session where username = upper('{checker}')"),
        "0",
    )
    .await;
    script(&dba, &format!("drop user {checker} cascade")).await;
    dba.close().await;
}
