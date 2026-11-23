//! A plain `pg_dump` file runs through the session (SDB-162): its `COPY … FROM stdin` rows,
//! kept apart from the statements by the dialect, are loaded through the import's path
//! (`Connection::insert_rows`), streamed a chunk at a time, in a transaction of their own
//! unless the user's is open. Against the Docker PostgreSQL (`SPELAION_TEST_PG`), through the
//! PostgreSQL plugin; `pg_dump` runs from the test server's own image with `docker run` (or
//! from `SPELAION_TEST_DB_TOOLS`, a folder), as `tests/backup.rs` does.

use crate::engines;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{CoreEvent, ResultStore, Risk, Session, pick_statements, script_risks};
use spelaion_driver_api::{ConnectParams, Value};

fn server() -> Option<(String, u16)> {
    let Ok(addr) = std::env::var("SPELAION_TEST_PG") else {
        eprintln!("SPELAION_TEST_PG not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

async fn session((host, port): &(String, u16), database: &str) -> Session {
    let driver = engines::postgres();
    let params = ConnectParams {
        host: host.clone(),
        port: *port,
        user: "spelaion".into(),
        password: Some(spelaion_driver_api::Secret::new("spelaion")),
        database: database.into(),
        ..Default::default()
    };
    let conn = driver.connect(&params).await.expect("connecting");
    Session::new(
        conn,
        spelaion_dialect::for_engine("postgres").expect("a dialect"),
        Arc::new(ResultStore::new()),
    )
}

/// Runs `script`; its events, after checking that no statement failed.
async fn run_ok(session: &Session, script: &str) -> Vec<CoreEvent> {
    let events: Vec<CoreEvent> = tokio::time::timeout(
        Duration::from_secs(300),
        session.run_script(script).collect(),
    )
    .await
    .expect("the script ended");
    for event in &events {
        if let CoreEvent::StatementFailed { index, error } = event {
            panic!("statement {index} failed: {}", error.message);
        }
    }
    events
}

async fn value(session: &Session, sql: &str) -> String {
    let mut events = session.run_script(sql);
    let mut last = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result, .. } => last = Some(result),
            CoreEvent::StatementFailed { error, .. } => panic!("{sql}: {}", error.message),
            _ => {}
        }
    }
    let stored = session
        .results()
        .get(last.expect("a result"))
        .expect("stored");
    let cell = stored.read().unwrap().value_at(0, 0).expect("a cell");
    match cell {
        Value::Text(text) => text,
        other => other.to_string(),
    }
}

/// A database of its own, empty.
async fn fresh_database(server: &(String, u16), name: &str) {
    let admin = session(server, "spelaion").await;
    run_ok(
        &admin,
        &format!("drop database if exists {name} with (force)"),
    )
    .await;
    run_ok(&admin, &format!("create database {name}")).await;
}

async fn drop_database(server: &(String, u16), name: &str) {
    let admin = session(server, "spelaion").await;
    let _ = admin
        .run_script(&format!("drop database if exists {name} with (force)"))
        .collect::<Vec<_>>()
        .await;
}

/// A plain `pg_dump` of `database`, without the random key of its `\restrict` lines.
fn pg_dump((host, port): &(String, u16), database: &str) -> String {
    let mut command = match std::env::var("SPELAION_TEST_DB_TOOLS") {
        Ok(folder) => std::process::Command::new(PathBuf::from(folder).join("pg_dump")),
        Err(_) => {
            let mut command = std::process::Command::new("docker");
            command.args(["run", "--rm", "-e", "PGPASSWORD"]);
            if cfg!(target_os = "linux") {
                command.args(["--network", "host"]);
            }
            command.arg("postgis/postgis:17-3.5-alpine").arg("pg_dump");
            command
        }
    };
    let host = if std::env::var("SPELAION_TEST_DB_TOOLS").is_err() && !cfg!(target_os = "linux") {
        "host.docker.internal".to_string()
    } else {
        host.clone()
    };
    let output = command
        .env("PGPASSWORD", "spelaion")
        .args([
            "-h",
            &host,
            "-p",
            &port.to_string(),
            "-U",
            "spelaion",
            "-d",
            database,
        ])
        .args(["--no-owner", "--no-privileges"])
        .output()
        .expect("running pg_dump");
    assert!(
        output.status.success(),
        "pg_dump: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("a UTF-8 dump")
        .lines()
        .filter(|line| !line.starts_with("\\restrict ") && !line.starts_with("\\unrestrict "))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// Tables with every common type, the awkward values COPY must carry, a generated column
/// (left out of the dump's column list), a foreign key, a view and a sequence.
const SOURCE: &str = r#"
create type mood as enum ('sad', 'ok', 'happy');
create table every_type (
    id serial primary key,
    b boolean, si smallint, i integer, bi bigint, r real, d double precision,
    n numeric(20, 6), t text, vc varchar(30), c char(5), by bytea,
    dt date, tm time, tmz timetz, ts timestamp, tsz timestamptz, iv interval,
    u uuid, j json, jb jsonb, arr integer[], tarr text[],
    ip inet, cid cidr, mac macaddr, m mood, pt point, rng int4range, bits bit varying(8),
    twice integer generated always as (si * 2) stored
);
insert into every_type (b, si, i, bi, r, d, n, t, vc, c, by, dt, tm, tmz, ts, tsz, iv, u, j, jb,
    arr, tarr, ip, cid, mac, m, pt, rng, bits) values
  (true, -32768, 2147483647, -9223372036854775808, 1.5, 3.141592653589793, 12345678.123456,
   E'tab\there\nnew line \\ backslash ''quote'' "dq" ; -- not SQL \\. é ✓ 🎉', 'varchar', 'ab',
   '\x00ff5c0a'::bytea, '2024-02-29', '23:59:59.999999', '12:00:00+05:30',
   '2024-01-01 00:00:00', '2024-06-30 12:34:56.789+02', '1 year 2 mons 3 days 04:05:06.7',
   '0e37a7c8-3d3e-4d1f-9b52-6a1b7c2d9e10', '{"a": [1, "x\ty"]}', '{"k": null, "n": 1.0}',
   '{1,NULL,3}', '{"a b","c\"d",NULL,""}', '192.168.0.1/24', '10.0.0.0/8', '08:00:2b:01:02:03',
   'happy', '(1.5,-2)', '[1,10)', B'1011'),
  (null, null, null, null, null, null, null, null, null, null, null, null, null, null, null,
   null, null, null, null, null, null, null, null, null, null, null, null, null, null),
  (false, 0, 0, 0, 'NaN', '-Infinity', 0, '', '', '', ''::bytea, 'infinity', '00:00', '00:00+00',
   '-infinity', 'infinity', '0', '00000000-0000-0000-0000-000000000000', 'null', '[]',
   '{}', '{}', '::1', '::/0', '00:00:00:00:00:00', 'sad', '(0,0)', 'empty', B'');
create table child (id integer primary key, parent integer references every_type (id), note text);
insert into child values (1, 1, 'first'), (2, 1, E'\\N is not NULL here'), (3, null, null);
create view pairs as select c.id, c.note, e.t from child c left join every_type e on e.id = c.parent;
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_plain_pg_dump_runs_through_the_session_and_dumps_again_identically() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(server) = server() else { return };
    let (source, target) = (
        format!("sdb162_src_{}", std::process::id()),
        format!("sdb162_dst_{}", std::process::id()),
    );
    fresh_database(&server, &source).await;
    fresh_database(&server, &target).await;
    run_ok(&session(&server, &source).await, SOURCE).await;
    let dump = pg_dump(&server, &source);
    assert!(
        dump.contains("FROM stdin;"),
        "a dump with COPY data:\n{dump}"
    );

    // The whole file, as the editor or "Run an .sql file" would send it (its `\restrict`
    // lines put back, as pg_dump writes them).
    let file = format!("\\restrict k3y\n{dump}\\unrestrict k3y\n");
    let restored = session(&server, &target).await;
    let events = run_ok(&restored, &file).await;
    let copied: Vec<u64> = events
        .iter()
        .filter_map(|event| match event {
            CoreEvent::StatementFinished {
                rows_affected: Some(rows),
                index,
                ..
            } if file_statement(&file, *index).contains("FROM stdin") => Some(*rows),
            _ => None,
        })
        .collect();
    assert_eq!(copied, [3, 3], "the rows each COPY loaded");
    drop(restored);

    let again = pg_dump(&server, &target);
    assert_eq!(again, dump, "the restored database dumps identically");
    drop_database(&server, &source).await;
    drop_database(&server, &target).await;
}

/// The text of statement `index` of `script`, as the session splits it.
fn file_statement(script: &str, index: usize) -> String {
    let dialect = spelaion_dialect::for_engine("postgres").unwrap();
    dialect.split_statements(script)[index]
        .text(script)
        .to_string()
}

/// A `COPY` of `rows` rows into `table`.
fn big_copy(table: &str, rows: usize) -> String {
    let mut script = format!("COPY {table} (id, v) FROM stdin;\n");
    script.reserve(rows * 20);
    for n in 1..=rows {
        script.push_str(&format!("{n}\trow {n}\n"));
    }
    script.push_str("\\.\n");
    script
}

#[tokio::test(flavor = "multi_thread")]
async fn a_million_rows_stream_and_a_cancel_half_way_leaves_nothing() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(server) = server() else { return };
    let database = format!("sdb162_big_{}", std::process::id());
    fresh_database(&server, &database).await;
    let writer = Arc::new(session(&server, &database).await);
    run_ok(&writer, "create table big (id integer, v text)").await;
    let script = big_copy("big", 1_000_000);

    let events = run_ok(&writer, &script).await;
    assert!(
        events.iter().any(|event| matches!(
            event,
            CoreEvent::StatementFinished {
                rows_affected: Some(1_000_000),
                ..
            }
        )),
        "the row count is reported"
    );
    assert_eq!(value(&writer, "select count(*) from big").await, "1000000");

    // Hold the COPY at the halfway point so cancel always reaches an in-flight load.
    let watcher = session(&server, &database).await;
    run_ok(&watcher, "select pg_advisory_lock(431)").await;
    run_ok(
        &writer,
        r#"
truncate big;
create function wait_at_halfway() returns trigger language plpgsql as $$
begin
    perform pg_advisory_xact_lock(431);
    return new;
end;
$$;
create trigger wait_at_halfway before insert on big
for each row when (new.id = 500001) execute function wait_at_halfway();
"#,
    )
    .await;
    let run = tokio::spawn({
        let writer = writer.clone();
        async move { writer.run_script(&script).collect::<Vec<_>>().await }
    });
    tokio::time::timeout(Duration::from_secs(120), async {
        let waiting = "select count(*) from pg_stat_activity where datname = current_database() \
                       and wait_event_type = 'Lock' and wait_event = 'advisory'";
        while value(&watcher, waiting).await == "0" {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the COPY reached the halfway lock");
    let events = tokio::time::timeout(Duration::from_secs(30), async {
        writer.cancel().await.unwrap();
        run.await.unwrap()
    })
    .await
    .expect("the cancel was obeyed in time");
    assert!(
        events.iter().any(|event| matches!(
            event,
            CoreEvent::StatementFailed { error, .. } if error.cancelled
        )),
        "cancelled: {:?}",
        events
            .iter()
            .filter(|e| !matches!(e, CoreEvent::Rows { .. }))
            .collect::<Vec<_>>()
    );
    // The cancelled COPY rolls back the transaction, including chunks already inserted.
    assert_eq!(value(&writer, "select count(*) from big").await, "0");
    drop((writer, watcher));
    drop_database(&server, &database).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn in_the_users_transaction_and_on_a_read_only_session() {
    let _turn = crate::serial::file_turn(module_path!());
    let Some(server) = server() else { return };
    let database = format!("sdb162_tx_{}", std::process::id());
    fresh_database(&server, &database).await;
    let writer = session(&server, &database).await;
    run_ok(&writer, "create table small (id integer, v text)").await;
    // Inside the user's transaction the rows are part of it: rolled back with it.
    run_ok(
        &writer,
        "begin;\nCOPY small (id, v) FROM stdin;\n1\ta\n\\.\nrollback;",
    )
    .await;
    assert_eq!(value(&writer, "select count(*) from small").await, "0");
    run_ok(
        &writer,
        "begin;\nCOPY small (id, v) FROM stdin;\n1\ta\n2\tb\n\\.\ncommit;",
    )
    .await;
    assert_eq!(value(&writer, "select count(*) from small").await, "2");

    // A read-only session refuses the COPY (a write) before anything is sent.
    let reader = session(&server, &database).await.with_read_only(true);
    let events: Vec<CoreEvent> = reader
        .run_script("COPY small (id, v) FROM stdin;\n3\tc\n\\.\n")
        .collect()
        .await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, CoreEvent::StatementFailed { .. })),
        "refused: {events:?}"
    );
    assert_eq!(value(&writer, "select count(*) from small").await, "2");

    // A COPY whose data does not fit its columns loads nothing.
    let events: Vec<CoreEvent> = writer
        .run_script("COPY small (id, v) FROM stdin;\n4\td\n5\n\\.\n")
        .collect()
        .await;
    let failed = events.iter().find_map(|e| match e {
        CoreEvent::StatementFailed { error, .. } => Some(error.message.clone()),
        _ => None,
    });
    assert!(
        failed.as_deref().is_some_and(|m| m.contains("line 2")),
        "{failed:?}"
    );
    assert_eq!(value(&writer, "select count(*) from small").await, "2");

    // psql's `\copy` is a meta-command, not a COPY (SDB-437): it is refused before anything is
    // sent, and the lines after it are never loaded as its data.
    let events: Vec<CoreEvent> = writer
        .run_script("\\copy small (id, v) from stdin;\n;\n6\tf\n\\.\n")
        .collect()
        .await;
    let failed = events.iter().find_map(|e| match e {
        CoreEvent::StatementFailed { index: 0, error } => Some(error.message.clone()),
        _ => None,
    });
    assert!(
        failed
            .as_deref()
            .is_some_and(|m| m.contains("psql command")),
        "{events:?}"
    );
    assert_eq!(value(&writer, "select count(*) from small").await, "2");
    drop((writer, reader));
    drop_database(&server, &database).await;
}

#[test]
fn the_guard_reads_the_copy_like_an_insert_and_never_its_data() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = spelaion_dialect::for_engine("postgres").unwrap();
    // The data holds what would be risky SQL: none of it is read.
    let script = "COPY t (a) FROM stdin;\ndrop table t;\ndelete from t\n\\.\nselect 1;";
    let production = Safeguards {
        confirm_data_changes: true,
        ..Safeguards::default()
    };
    let risks = script_risks(dialect.as_ref(), script, &production);
    assert_eq!(risks.len(), 1, "{risks:?}");
    assert_eq!((risks[0].index, risks[0].risk), (0, Risk::DataChange));
    assert!(script_risks(dialect.as_ref(), script, &Safeguards::default()).is_empty());

    // Running from a statement or a selection keeps a COPY's data with it, and drops it with
    // a COPY that is left out.
    let picked = pick_statements(dialect.as_ref(), script, &[0]).unwrap();
    assert!(
        picked.text.contains("drop table t;\ndelete from t\n\\."),
        "{}",
        picked.text
    );
    let after = pick_statements(dialect.as_ref(), script, &[1]).unwrap();
    assert!(!after.text.contains("drop table"), "{}", after.text);
    assert_eq!(dialect.split_statements(&after.text).len(), 1);
}
