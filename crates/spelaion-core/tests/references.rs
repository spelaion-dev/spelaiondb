//! Foreign keys followed both ways on every engine: SQLite and DuckDB always, PostgreSQL,
//! MySQL, MariaDB and SQL Server with their `SPELAION_TEST_*` variables (without them they
//! are skipped, which does not count as tested).
//!
//! The schema has a composite key (an order line points to its order by region and number, a
//! column name with a space among them), three levels (line → order → customer → country) and
//! a customer that refers another customer. From a row of a table data page each key is
//! followed to the rows it leads to, and the filter the core writes is run as a page of the
//! other table: it must keep exactly the rows the key relates.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_core::{
    CoreEvent, FollowError, KeyDirection, KeyRef, ReferenceOutcome, ResultId, ResultStore, Session,
    TableKey, TablePage, find_key, follow, key_rows, read_references, reference_targets,
    table_keys, table_page_sql,
};
use spelaion_driver_api::{ConnectParams, Driver, Secret, TlsMode};

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
    let memory = || ConnectParams {
        database: ":memory:".into(),
        ..Default::default()
    };
    let mut out = vec![
        (
            "sqlite",
            session(&spelaion_driver_sqlite::SqliteDriver::default(), memory()).await,
            "main",
        ),
        (
            "duckdb",
            session(&spelaion_driver_duckdb::DuckdbDriver::default(), memory()).await,
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

/// Runs each statement on its own; panics on a failure unless `tolerate`. Returns the result
/// of the last statement that produced one (kept in the session's store).
async fn run(
    label: &str,
    session: &Session,
    statements: &[String],
    tolerate: bool,
) -> Option<ResultId> {
    let mut last = None;
    for statement in statements {
        let events: Vec<CoreEvent> = session.run_script(statement).collect().await;
        for event in &events {
            match event {
                CoreEvent::StatementFailed { error, .. } if !tolerate => {
                    panic!("{label}: {statement}\n→ {}", error.message)
                }
                CoreEvent::ResultStarted { result, .. } => last = Some(*result),
                _ => {}
            }
        }
    }
    last
}

const COUNTRY: &str = "fkf_country";
const CUSTOMER: &str = "fkf_customer";
const ORDER: &str = "fkf_order";
const LINE: &str = "fkf_line";

/// Creates the four tables and their rows (children dropped first).
async fn create_schema(label: &str, session: &Session) {
    let q = |name: &str| session.dialect().quote_identifier(name);
    let region = q("order region");
    let drops: Vec<String> = [LINE, ORDER, CUSTOMER, COUNTRY]
        .iter()
        .map(|t| format!("DROP TABLE {t}"))
        .collect();
    run(label, session, &drops, true).await;
    let statements = vec![
        format!("CREATE TABLE {COUNTRY} (code CHAR(2) PRIMARY KEY, name VARCHAR(40))"),
        format!(
            "CREATE TABLE {CUSTOMER} (id INT PRIMARY KEY, name VARCHAR(40), \
             country CHAR(2), referrer INT, \
             FOREIGN KEY (country) REFERENCES {COUNTRY} (code), \
             FOREIGN KEY (referrer) REFERENCES {CUSTOMER} (id))"
        ),
        format!(
            "CREATE TABLE {ORDER} ({region} VARCHAR(10) NOT NULL, number INT NOT NULL, \
             customer INT, PRIMARY KEY ({region}, number), \
             FOREIGN KEY (customer) REFERENCES {CUSTOMER} (id))"
        ),
        format!(
            "CREATE TABLE {LINE} (id INT PRIMARY KEY, {region} VARCHAR(10), number INT, \
             product VARCHAR(40), \
             CONSTRAINT fkf_line_order FOREIGN KEY ({region}, number) \
             REFERENCES {ORDER} ({region}, number))"
        ),
        format!("INSERT INTO {COUNTRY} (code, name) VALUES ('ES', 'Spain'), ('FR', 'France')"),
        // Ana first: DuckDB checks a key against the rows before the statement.
        format!(
            "INSERT INTO {CUSTOMER} (id, name, country, referrer) VALUES (1, 'Ana', 'ES', NULL)"
        ),
        format!(
            "INSERT INTO {CUSTOMER} (id, name, country, referrer) VALUES \
             (2, 'Luc', 'FR', 1), (3, 'Eva', 'ES', 1), (4, 'Nobody', NULL, NULL)"
        ),
        // Two orders share a number in different regions: the key needs both columns.
        format!(
            "INSERT INTO {ORDER} ({region}, number, customer) VALUES \
             ('EU', 7, 1), ('US', 7, 2), ('EU', 8, 3)"
        ),
        format!(
            "INSERT INTO {LINE} (id, {region}, number, product) VALUES \
             (1, 'EU', 7, 'pen'), (2, 'EU', 7, 'ink'), (3, 'US', 7, 'cap'), (4, NULL, NULL, 'loose')"
        ),
    ];
    run(label, session, &statements, false).await;
}

/// A page of `schema.table` (ordered by `order_by`, a quoted column) loaded in the session's
/// store, as a table data tab loads it.
async fn page(
    label: &str,
    session: &Session,
    schema: &str,
    table: &str,
    filter: &str,
    order_by: &str,
) -> ResultId {
    let sql = table_page_sql(
        session.dialect().as_ref(),
        &TablePage {
            schema: schema.into(),
            name: table.into(),
            filter: filter.into(),
            order_by: session.dialect().quote_identifier(order_by),
            limit: 100,
            offset: 0,
        },
    )
    .unwrap_or_else(|e| panic!("{label}: page of {table}: {e}"));
    run(label, session, &[sql], false)
        .await
        .unwrap_or_else(|| panic!("{label}: no result for {table}"))
}

/// The text of every cell of `column` in a stored result, in order.
fn column_text(session: &Session, result: ResultId, column: &str) -> Vec<String> {
    let stored = session.results().get(result).expect("result");
    let stored = stored.read().unwrap();
    let index = stored
        .schema()
        .columns
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(column))
        .unwrap_or_else(|| panic!("no column {column}"));
    (0..stored.rows())
        .map(|row| {
            stored
                .value_at(row, index)
                .map(|v| v.to_string())
                .unwrap_or_default()
                .trim_end()
                .to_string()
        })
        .collect()
}

/// The key named by its direction and other table.
async fn key(
    label: &str,
    session: &Session,
    schema: &str,
    table: &str,
    direction: KeyDirection,
    other: &str,
) -> TableKey {
    let keys = table_keys(session.connection().as_ref(), schema, table)
        .await
        .unwrap_or_else(|e| panic!("{label}: keys of {table}: {e}"));
    keys.into_iter()
        .find(|k| k.direction == direction && k.table.eq_ignore_ascii_case(other))
        .unwrap_or_else(|| panic!("{label}: {table} has no {direction:?} key with {other}"))
}

/// Follows `key` from the row at `row` of `result` and loads where it leads, ordered by
/// `order_by`.
async fn follow_to(
    label: &str,
    session: &Session,
    result: ResultId,
    row: usize,
    key: &TableKey,
    order_by: &str,
) -> ResultId {
    let target = {
        let stored = session.results().get(result).expect("result");
        let stored = stored.read().unwrap();
        follow(session.dialect().as_ref(), &stored, row, key)
            .unwrap_or_else(|e| panic!("{label}: following {}: {e}", key.name))
    };
    assert!(target.table.eq_ignore_ascii_case(&key.table), "{label}");
    page(
        label,
        session,
        &target.schema,
        &target.table,
        &target.filter,
        order_by,
    )
    .await
}

#[tokio::test]
async fn foreign_keys_are_followed_both_ways_on_every_engine() {
    for (label, session, schema) in engines().await {
        create_schema(label, &session).await;
        let conn = session.connection().as_ref();

        // The catalog, both ways: the composite key keeps its order on both sides.
        let to_order = key(label, &session, schema, LINE, KeyDirection::Outgoing, ORDER).await;
        assert_eq!(to_order.columns, ["order region", "number"], "{label}");
        assert_eq!(
            to_order.other_columns,
            ["order region", "number"],
            "{label}"
        );
        assert_eq!(to_order.schema, schema, "{label}");
        let from_lines = key(label, &session, schema, ORDER, KeyDirection::Incoming, LINE).await;
        assert_eq!(from_lines.name, to_order.name, "{label}");
        assert_eq!(from_lines.columns, ["order region", "number"], "{label}");
        assert_eq!(from_lines.schema, schema, "{label}");
        let order_keys = table_keys(conn, schema, ORDER).await.unwrap();
        assert_eq!(
            order_keys
                .iter()
                .map(|k| (k.direction, k.table.to_lowercase()))
                .collect::<Vec<_>>(),
            [
                (KeyDirection::Outgoing, CUSTOMER.to_string()),
                (KeyDirection::Incoming, LINE.to_string())
            ],
            "{label}"
        );
        // A table that points to itself lists that key both ways; a table nobody references
        // has no incoming keys.
        let customer_keys = table_keys(conn, schema, CUSTOMER).await.unwrap();
        let self_keys = customer_keys
            .iter()
            .filter(|k| k.table.eq_ignore_ascii_case(CUSTOMER))
            .count();
        assert_eq!(self_keys, 2, "{label}: {customer_keys:?}");
        assert!(
            table_keys(conn, schema, LINE)
                .await
                .unwrap()
                .iter()
                .all(|k| k.direction == KeyDirection::Outgoing),
            "{label}"
        );

        // Down three levels: line 3 → order (US, 7) → customer 2 → France.
        let lines = page(label, &session, schema, LINE, "", "id").await;
        let order = follow_to(label, &session, lines, 2, &to_order, "number").await;
        assert_eq!(
            column_text(&session, order, "order region"),
            ["US"],
            "{label}"
        );
        assert_eq!(column_text(&session, order, "number"), ["7"], "{label}");
        let to_customer = key(
            label,
            &session,
            schema,
            ORDER,
            KeyDirection::Outgoing,
            CUSTOMER,
        )
        .await;
        let customer = follow_to(label, &session, order, 0, &to_customer, "id").await;
        assert_eq!(column_text(&session, customer, "name"), ["Luc"], "{label}");
        let to_country = key(
            label,
            &session,
            schema,
            CUSTOMER,
            KeyDirection::Outgoing,
            COUNTRY,
        )
        .await;
        let country = follow_to(label, &session, customer, 0, &to_country, "code").await;
        assert_eq!(
            column_text(&session, country, "name"),
            ["France"],
            "{label}"
        );

        // And back up: Spain → its customers → Ana's orders → order (EU, 7)'s lines.
        let countries = page(label, &session, schema, COUNTRY, "", "code").await;
        let from_customers = key(
            label,
            &session,
            schema,
            COUNTRY,
            KeyDirection::Incoming,
            CUSTOMER,
        )
        .await;
        let spaniards = follow_to(label, &session, countries, 0, &from_customers, "id").await;
        assert_eq!(
            column_text(&session, spaniards, "name"),
            ["Ana", "Eva"],
            "{label}"
        );
        let from_orders = key(
            label,
            &session,
            schema,
            CUSTOMER,
            KeyDirection::Incoming,
            ORDER,
        )
        .await;
        let anas = follow_to(label, &session, spaniards, 0, &from_orders, "number").await;
        assert_eq!(column_text(&session, anas, "number"), ["7"], "{label}");
        assert_eq!(
            column_text(&session, anas, "order region"),
            ["EU"],
            "{label}"
        );
        // Only the lines of (EU, 7), not those of (US, 7): both columns are compared.
        let order_lines = follow_to(label, &session, anas, 0, &from_lines, "id").await;
        assert_eq!(
            column_text(&session, order_lines, "product"),
            ["pen", "ink"],
            "{label}"
        );

        // The key to itself, both ways: Luc was referred by Ana, who referred Luc and Eva.
        let referrer = customer_keys
            .iter()
            .find(|k| {
                k.direction == KeyDirection::Outgoing && k.table.eq_ignore_ascii_case(CUSTOMER)
            })
            .unwrap()
            .clone();
        let referred = customer_keys
            .iter()
            .find(|k| {
                k.direction == KeyDirection::Incoming && k.table.eq_ignore_ascii_case(CUSTOMER)
            })
            .unwrap()
            .clone();
        let customers = page(label, &session, schema, CUSTOMER, "", "id").await;
        let anas_page = follow_to(label, &session, customers, 1, &referrer, "id").await;
        assert_eq!(column_text(&session, anas_page, "name"), ["Ana"], "{label}");
        let by_ana = follow_to(label, &session, customers, 0, &referred, "id").await;
        assert_eq!(
            column_text(&session, by_ana, "name"),
            ["Luc", "Eva"],
            "{label}"
        );

        // A NULL key references nothing: following it is refused, saying which column.
        {
            let stored = session.results().get(lines).unwrap();
            let stored = stored.read().unwrap();
            let refused = follow(session.dialect().as_ref(), &stored, 3, &to_order);
            assert!(
                matches!(&refused, Err(FollowError::Null { column, .. }) if column == "order region"),
                "{label}: {refused:?}"
            );
            assert_eq!(
                follow(session.dialect().as_ref(), &stored, 99, &to_order),
                Err(FollowError::NoSuchRow),
                "{label}"
            );
        }

        // The UI names a key; the core finds it in the catalog again, or says it is gone.
        let named = KeyRef {
            direction: KeyDirection::Incoming,
            schema: from_lines.schema.clone(),
            table: from_lines.table.clone(),
            name: from_lines.name.clone(),
        };
        assert_eq!(
            find_key(conn, schema, ORDER, &named).await.unwrap(),
            from_lines,
            "{label}"
        );
        let gone = KeyRef {
            name: "fkf_no_such_key".into(),
            ..named.clone()
        };
        assert!(
            matches!(
                find_key(conn, schema, ORDER, &gone).await,
                Err(FollowError::NoSuchKey(_))
            ),
            "{label}"
        );
        // Asked for the wrong way round, the same name is not found either.
        let wrong_way = KeyRef {
            direction: KeyDirection::Outgoing,
            ..named
        };
        assert!(
            find_key(conn, schema, ORDER, &wrong_way).await.is_err(),
            "{label}"
        );

        // The references panel: every key of a row, its first rows, and NULL keys explained.
        let orders = page(label, &session, schema, ORDER, "", "customer").await;
        let targets = {
            let stored = session.results().get(orders).unwrap();
            let stored = stored.read().unwrap();
            reference_targets(session.dialect().as_ref(), &stored, 0, order_keys.clone())
        };
        let panel = read_references(&session, targets, 1).await;
        assert_eq!(panel.len(), 2, "{label}");
        let ReferenceOutcome::Rows(_, customer_rows) = &panel[0].outcome else {
            panic!("{label}: {:?}", panel[0].outcome);
        };
        assert_eq!(customer_rows.rows.len(), 1, "{label}");
        assert!(!customer_rows.more, "{label}");
        let ReferenceOutcome::Rows(target, line_rows) = &panel[1].outcome else {
            panic!("{label}: {:?}", panel[1].outcome);
        };
        assert_eq!(line_rows.rows.len(), 1, "{label}: one row asked for");
        assert!(line_rows.more, "{label}: (EU, 7) has two lines");
        let all = key_rows(&session, target, 10).await.unwrap();
        assert_eq!(all.rows.len(), 2, "{label}");
        assert!(!all.more, "{label}");
        let product = all
            .columns
            .iter()
            .position(|c| c.eq_ignore_ascii_case("product"))
            .unwrap();
        assert_eq!(all.rows[0][product].as_deref(), Some("pen"), "{label}");

        let customers_targets = {
            let stored = session.results().get(customers).unwrap();
            let stored = stored.read().unwrap();
            // Nobody (row 3) has no country and no referrer.
            reference_targets(
                session.dialect().as_ref(),
                &stored,
                3,
                customer_keys.clone(),
            )
        };
        let panel = read_references(&session, customers_targets, 5).await;
        for reference in &panel {
            match reference.key.direction {
                KeyDirection::Outgoing => assert!(
                    matches!(&reference.outcome, ReferenceOutcome::Null { .. }),
                    "{label}: {:?}",
                    reference.outcome
                ),
                KeyDirection::Incoming => assert!(
                    matches!(&reference.outcome, ReferenceOutcome::Rows(_, rows) if rows.rows.is_empty()),
                    "{label}: {:?}",
                    reference.outcome
                ),
            }
        }

        let drops: Vec<String> = [LINE, ORDER, CUSTOMER, COUNTRY]
            .iter()
            .map(|t| format!("DROP TABLE {t}"))
            .collect();
        run(label, &session, &drops, false).await;
    }
}
