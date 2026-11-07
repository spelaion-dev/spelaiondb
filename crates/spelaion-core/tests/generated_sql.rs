//! The SQL the explorer generates (`table_sql`) runs on every engine once its `:placeholders`
//! are filled in: SQLite and DuckDB always, PostgreSQL, MySQL, MariaDB and SQL Server with
//! their `SPELAION_TEST_*` variables (without them they are skipped, which does not count as
//! tested).

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, ResultStore, Session, SqlTemplate, join_sql, table_sql, table_templates,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode, Value};

async fn session(driver: &dyn Driver, params: ConnectParams) -> Session {
    let conn = driver
        .connect(&params)
        .await
        .unwrap_or_else(|e| panic!("connecting: {e}"));
    Session::new(conn, driver.dialect(), Arc::new(ResultStore::new()))
}

fn server(var: &str) -> Option<(String, u16)> {
    let Ok(addr) = std::env::var(var) else {
        eprintln!("{var} not set: skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').expect("host:port");
    Some((host.to_string(), port.parse().expect("port")))
}

/// Every engine available here, with the schema its tables go in.
async fn engines() -> Vec<(&'static str, Session, &'static str)> {
    let mut out = vec![
        (
            "sqlite",
            session(
                &spelaion_driver_sqlite::SqliteDriver::default(),
                ConnectParams {
                    database: ":memory:".into(),
                    ..Default::default()
                },
            )
            .await,
            "main",
        ),
        (
            "duckdb",
            session(
                &spelaion_driver_duckdb::DuckdbDriver::default(),
                ConnectParams {
                    database: ":memory:".into(),
                    ..Default::default()
                },
            )
            .await,
            "main",
        ),
    ];
    let login =
        |host: String, port: u16, user: &str, password: &str, database: &str| ConnectParams {
            host,
            port,
            user: user.into(),
            password: Some(Secret::new(password)),
            database: database.into(),
            ..Default::default()
        };
    if let Some((host, port)) = server("SPELAION_TEST_PG") {
        let params = login(host, port, "spelaion", "spelaion", "spelaion");
        out.push((
            "postgres",
            session(&spelaion_driver_postgres::PostgresDriver, params).await,
            "public",
        ));
    }
    for (label, var, driver) in [
        (
            "mysql",
            "SPELAION_TEST_MYSQL",
            spelaion_driver_mysql::MysqlDriver::mysql(),
        ),
        (
            "mariadb",
            "SPELAION_TEST_MARIADB",
            spelaion_driver_mysql::MysqlDriver::mariadb(),
        ),
    ] {
        if let Some((host, port)) = server(var) {
            let params = login(host, port, "spelaion", "spelaion", "spelaion");
            out.push((label, session(&driver, params).await, "spelaion"));
        }
    }
    if let Some((host, port)) = server("SPELAION_TEST_MSSQL") {
        // tempdb always exists: no other test has to create a database first.
        let params = ConnectParams {
            tls: TlsMode::Require,
            ..login(host, port, "sa", "Spelaion-test-1", "tempdb")
        };
        out.push((
            "sqlserver",
            session(&spelaion_driver_sqlserver::SqlServerDriver, params).await,
            "dbo",
        ));
    }
    out
}

/// Runs a script; panics on the first failure. Returns the rows of each result, in order.
async fn run(label: &str, session: &Session, script: &str) -> Vec<usize> {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let mut rows = Vec::new();
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => {
                panic!("{label}: {script}\n→ {}", error.message)
            }
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(*result).unwrap();
                rows.push(stored.read().unwrap().rows());
                session.results().remove(*result);
            }
            _ => {}
        }
    }
    rows
}

/// The first cell of a query, as text.
async fn first(label: &str, session: &Session, sql: &str) -> String {
    let events: Vec<CoreEvent> = session.run_script(sql).collect().await;
    let result = events
        .iter()
        .find_map(|e| match e {
            CoreEvent::ResultStarted { result, .. } => Some(*result),
            _ => None,
        })
        .unwrap_or_else(|| panic!("{label}: {sql}: {events:?}"));
    let stored = session.results().get(result).unwrap();
    let text = {
        let stored = stored.read().unwrap();
        let batch = &stored.window(0, 1)[0];
        Value::from_array(
            batch.column(0).as_ref(),
            0,
            stored.schema().columns[0].logical,
        )
        .unwrap()
        .to_string()
    };
    session.results().remove(result);
    text
}

/// The generated SQL with its `:placeholders` replaced by literals (longest names first, so
/// `:id_1` is not read as `:id` followed by `_1`).
fn fill(sql: &str, values: &[(&str, &str)]) -> String {
    let mut values = values.to_vec();
    values.sort_by_key(|(name, _)| std::cmp::Reverse(name.len()));
    let mut out = sql.to_string();
    for (name, literal) in values {
        out = out.replace(&format!(":{name}"), literal);
    }
    assert!(
        !out.contains(" :") && !out.contains("(:"),
        "a placeholder was left: {out}"
    );
    out
}

async fn generated(session: &Session, schema: &str, table: &str, template: SqlTemplate) -> String {
    let info = session
        .connection()
        .describe_table(schema, table)
        .await
        .unwrap_or_else(|e| panic!("{table}: {e}"));
    table_sql(session.dialect().as_ref(), &info, template)
}

#[tokio::test]
async fn selects_by_key_and_writes_by_key_run_everywhere() {
    let pid = std::process::id();
    let items = format!("gen_items_{pid}");
    let codes = format!("gen_codes_{pid}");
    for (label, session, schema) in engines().await {
        for sql in [
            format!("drop table if exists {items}"),
            format!("drop table if exists {codes}"),
            format!("create table {items} (id int primary key, name varchar(40), qty int)"),
            format!("create table {codes} (code varchar(10) not null unique, label varchar(40))"),
        ] {
            run(label, &session, &sql).await;
        }
        let insert = generated(&session, schema, &items, SqlTemplate::Insert).await;
        for (id, name, qty) in [("1", "'a'", "5"), ("2", "'b'", "6"), ("3", "'c'", "7")] {
            run(
                label,
                &session,
                &fill(&insert, &[("id", id), ("name", name), ("qty", qty)]),
            )
            .await;
        }

        let by_key = generated(&session, schema, &items, SqlTemplate::SelectByKey).await;
        assert_eq!(
            run(label, &session, &fill(&by_key, &[("id", "2")])).await,
            [1]
        );
        let in_list = generated(&session, schema, &items, SqlTemplate::SelectIn).await;
        let filled = fill(&in_list, &[("id_1", "1"), ("id_2", "3"), ("id_3", "99")]);
        assert_eq!(run(label, &session, &filled).await, [2], "{label}");

        let update = generated(&session, schema, &items, SqlTemplate::Update).await;
        run(
            label,
            &session,
            &fill(&update, &[("id", "1"), ("name", "'z'"), ("qty", "8")]),
        )
        .await;
        assert_eq!(
            first(
                label,
                &session,
                &format!("select name from {items} where id = 1")
            )
            .await,
            "z"
        );

        // Without a primary key, the unique key identifies the row to delete.
        let insert = generated(&session, schema, &codes, SqlTemplate::Insert).await;
        for (code, text) in [("'x'", "'first'"), ("'y'", "'second'")] {
            run(
                label,
                &session,
                &fill(&insert, &[("code", code), ("label", text)]),
            )
            .await;
        }
        let delete = generated(&session, schema, &codes, SqlTemplate::Delete).await;
        assert!(delete.contains("the unique key"), "{label}: {delete}");
        run(label, &session, &fill(&delete, &[("code", "'x'")])).await;
        assert_eq!(
            first(label, &session, &format!("select count(*) from {codes}")).await,
            "1",
            "{label}"
        );

        // A key of two columns: a list of rows (one condition per row joined by OR in T-SQL).
        let pairs = format!("gen_pairs_{pid}");
        for sql in [
            format!("drop table if exists {pairs}"),
            format!("create table {pairs} (a int not null, b int not null, primary key (a, b))"),
            format!("insert into {pairs} (a, b) values (1, 1), (1, 2), (2, 1)"),
        ] {
            run(label, &session, &sql).await;
        }
        let in_list = generated(&session, schema, &pairs, SqlTemplate::SelectIn).await;
        let filled = fill(
            &in_list,
            &[
                ("a_1", "1"),
                ("b_1", "2"),
                ("a_2", "2"),
                ("b_2", "1"),
                ("a_3", "9"),
                ("b_3", "9"),
            ],
        );
        assert_eq!(
            run(label, &session, &filled).await,
            [2],
            "{label}: {filled}"
        );

        run(label, &session, &format!("drop table {items}")).await;
        run(label, &session, &format!("drop table {codes}")).await;
        run(label, &session, &format!("drop table {pairs}")).await;
    }
}

#[tokio::test]
async fn upserts_and_merges_insert_then_update() {
    let pid = std::process::id();
    let items = format!("gen_upsert_{pid}");
    for (label, session, schema) in engines().await {
        let offered = table_templates(session.dialect().as_ref());
        for template in [SqlTemplate::Upsert, SqlTemplate::Merge] {
            if !offered.contains(&template) {
                continue;
            }
            for sql in [
                format!("drop table if exists {items}"),
                format!("create table {items} (id int primary key, name varchar(40), qty int)"),
            ] {
                run(label, &session, &sql).await;
            }
            let sql = generated(&session, schema, &items, template).await;
            // The first time it inserts, the second time it updates the same row.
            for (name, qty) in [("'a'", "1"), ("'b'", "2")] {
                run(
                    label,
                    &session,
                    &fill(&sql, &[("id", "7"), ("name", name), ("qty", qty)]),
                )
                .await;
            }
            assert_eq!(
                first(label, &session, &format!("select count(*) from {items}")).await,
                "1",
                "{label} {template:?}: {sql}"
            );
            assert_eq!(
                first(
                    label,
                    &session,
                    &format!("select name from {items} where id = 7")
                )
                .await,
                "b",
                "{label} {template:?}: {sql}"
            );
            run(label, &session, &format!("drop table {items}")).await;
        }
        let expected = match label {
            "sqlite" | "mysql" | "mariadb" => vec![SqlTemplate::Upsert],
            "sqlserver" => vec![SqlTemplate::Merge],
            _ => vec![SqlTemplate::Upsert, SqlTemplate::Merge],
        };
        let got: Vec<SqlTemplate> = offered
            .into_iter()
            .filter(|t| matches!(t, SqlTemplate::Upsert | SqlTemplate::Merge))
            .collect();
        assert_eq!(got, expected, "{label}");
    }
}

#[tokio::test]
async fn updates_from_and_deletes_using_a_source() {
    let pid = std::process::id();
    let items = format!("gen_from_{pid}");
    for (label, session, schema) in engines().await {
        for sql in [
            format!("drop table if exists {items}"),
            format!("create table {items} (id int primary key, name varchar(40), qty int)"),
            format!("insert into {items} (id, name, qty) values (1, 'a', 1), (2, 'b', 2)"),
        ] {
            run(label, &session, &sql).await;
        }
        let update = generated(&session, schema, &items, SqlTemplate::UpdateFrom).await;
        // What the app asks to confirm (an UPDATE without WHERE…) never applies: each has one.
        assert!(
            spelaion_core::script_risks(session.dialect().as_ref(), &update, &Default::default())
                .is_empty(),
            "{label}: {update}"
        );
        run(
            label,
            &session,
            &fill(&update, &[("id", "2"), ("name", "'z'"), ("qty", "9")]),
        )
        .await;
        assert_eq!(
            first(
                label,
                &session,
                &format!("select name from {items} where id = 2")
            )
            .await,
            "z",
            "{label}: {update}"
        );
        assert_eq!(
            first(
                label,
                &session,
                &format!("select name from {items} where id = 1")
            )
            .await,
            "a",
            "{label}: {update}"
        );
        if table_templates(session.dialect().as_ref()).contains(&SqlTemplate::DeleteUsing) {
            let delete = generated(&session, schema, &items, SqlTemplate::DeleteUsing).await;
            assert!(
                spelaion_core::script_risks(
                    session.dialect().as_ref(),
                    &delete,
                    &Default::default()
                )
                .is_empty(),
                "{label}: {delete}"
            );
            run(label, &session, &fill(&delete, &[("id", "1")])).await;
            assert_eq!(
                first(label, &session, &format!("select count(*) from {items}")).await,
                "1",
                "{label}: {delete}"
            );
        } else {
            assert_eq!(label, "sqlite");
        }
        run(label, &session, &format!("drop table {items}")).await;
    }
}

#[tokio::test]
async fn joins_between_chosen_tables_run_everywhere() {
    let pid = std::process::id();
    let (customer, orders, other) = (
        format!("gen_customer_{pid}"),
        format!("gen_orders_{pid}"),
        format!("gen_other_{pid}"),
    );
    for (label, session, schema) in engines().await {
        for sql in [
            format!("drop table if exists {orders}"),
            format!("drop table if exists {customer}"),
            format!("drop table if exists {other}"),
            format!("create table {customer} (id int primary key, name varchar(40))"),
            format!(
                // A table-level FOREIGN KEY: MySQL ignores REFERENCES written on a column.
                "create table {orders} (id int primary key, customer_id int, total int, \
                 foreign key (customer_id) references {customer} (id))"
            ),
            format!("create table {other} (code varchar(10))"),
            format!("insert into {customer} (id, name) values (1, 'a'), (2, 'b')"),
            format!(
                "insert into {orders} (id, customer_id, total) values (10, 1, 5), (11, 1, 6), (12, 2, 7)"
            ),
            format!("insert into {other} (code) values ('x'), ('y')"),
        ] {
            run(label, &session, &sql).await;
        }
        let mut tables = Vec::new();
        for name in [&orders, &customer, &other] {
            tables.push(
                session
                    .connection()
                    .describe_table(schema, name)
                    .await
                    .unwrap(),
            );
        }
        let dialect = session.dialect();
        for inner in [true, false] {
            let linked = join_sql(dialect.as_ref(), &tables[..2], inner);
            assert_eq!(
                run(label, &session, &linked).await,
                [3],
                "{label}: {linked}"
            );
            // The unlinked table multiplies the rows.
            let all = join_sql(dialect.as_ref(), &tables, inner);
            assert_eq!(run(label, &session, &all).await, [6], "{label}: {all}");
        }
        for name in [&orders, &customer, &other] {
            run(label, &session, &format!("drop table {name}")).await;
        }
    }
}
