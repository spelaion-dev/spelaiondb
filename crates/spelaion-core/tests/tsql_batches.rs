//! T-SQL variables live for one batch: a `DECLARE` and the statements after it, up to the
//! next `GO`, run as one batch from the editor. The checks around a run still see every
//! statement inside it. The run itself needs `SPELAION_TEST_MSSQL=127.0.0.1:11433`
//! (testing/docker); without it that test is skipped, which does not count as tested.

use std::sync::Arc;

use futures_util::StreamExt;
use spelaion_config::Safeguards;
use spelaion_core::{
    CoreEvent, ResultStore, Risk, Session, check_script, check_write, classify_read_only,
    statement_at,
};
use spelaion_driver_api::{ConnectParams, Dialect, Driver, Secret, TlsMode, Value};
use spelaion_driver_sqlserver::{SqlServerDialect, SqlServerDriver};

async fn session() -> Option<Session> {
    let Ok(addr) = std::env::var("SPELAION_TEST_MSSQL") else {
        eprintln!("SPELAION_TEST_MSSQL not set: test skipped");
        return None;
    };
    let (host, port) = addr.rsplit_once(':').unwrap();
    let driver = SqlServerDriver;
    let conn = driver
        .connect(&ConnectParams {
            host: host.into(),
            port: port.parse().unwrap(),
            user: "sa".into(),
            password: Some(Secret::new("Spelaion-test-1")),
            database: "tempdb".into(),
            tls: TlsMode::Require,
            ..Default::default()
        })
        .await
        .expect("connecting to SQL Server");
    Some(Session::new(
        conn,
        spelaion_dialect::for_engine(driver.engine()).expect("a dialect"),
        Arc::new(ResultStore::new()),
    ))
}

/// The first cell of every result the script returned, in order; panics on a failure.
async fn first_cells(session: &Session, script: &str) -> Vec<String> {
    let events: Vec<CoreEvent> = session.run_script(script).collect().await;
    let mut cells = Vec::new();
    for event in &events {
        match event {
            CoreEvent::StatementFailed { error, .. } => panic!("{script}: {error:?}"),
            CoreEvent::ResultStarted { result, .. } => {
                let stored = session.results().get(*result).unwrap();
                let stored = stored.read().unwrap();
                let batch = &stored.window(0, 1)[0];
                cells.push(
                    Value::from_array(
                        batch.column(0).as_ref(),
                        0,
                        stored.schema().columns[0].logical,
                    )
                    .unwrap()
                    .to_string(),
                );
            }
            _ => {}
        }
    }
    cells
}

#[tokio::test]
async fn a_declared_variable_is_known_to_the_statements_after_it() {
    let Some(session) = session().await else {
        return;
    };
    assert_eq!(
        first_cells(&session, "DECLARE @id int = 5;\nSELECT @id AS id;").await,
        ["5"]
    );
    // `SET` after its `DECLARE`, a table variable, and a statement before them all.
    assert_eq!(
        first_cells(
            &session,
            "select 1 as before_them;\n\
             declare @x int;\n\
             set @x = 41;\n\
             declare @t table (id int);\n\
             insert into @t values (@x + 1);\n\
             select id from @t;"
        )
        .await,
        ["1", "42"]
    );
    // The next batch does not know it.
    let events: Vec<CoreEvent> = session
        .run_script("declare @gone int = 1; select @gone;\nGO\nselect @gone;")
        .collect()
        .await;
    let failed = events.iter().find_map(|e| match e {
        CoreEvent::StatementFailed { error, .. } => Some(error.message.clone()),
        _ => None,
    });
    assert!(
        failed.as_deref().is_some_and(|m| m.contains("@gone")),
        "{events:?}"
    );
}

fn risk(script: &str) -> Option<Risk> {
    check_script(&SqlServerDialect, script, &Safeguards::default(), false)
        .err()
        .map(|e| e.risks[0].risk)
}

/// The batch is one statement for the editor, but the guard looks at each statement in it,
/// also when the parser cannot read some other statement of the batch.
#[test]
fn the_guard_still_sees_every_statement_of_the_batch() {
    assert_eq!(
        risk("declare @x int = 1; delete from t"),
        Some(Risk::DeleteAll)
    );
    assert_eq!(
        risk("declare @x int = 1;\nselect @x;\nupdate t set a = @x;"),
        Some(Risk::UpdateAll)
    );
    assert_eq!(
        risk("declare @t table (id int); insert into @t values (1); truncate table t;"),
        Some(Risk::Truncate)
    );
    // Statements sqlparser does not read, around the write.
    assert_eq!(
        risk(
            "declare @n int = 0;\nwhile @n < 3 begin set @n += 1; end;\nraiserror('x', 0, 1) with nowait;\ndelete from t;"
        ),
        Some(Risk::DeleteAll)
    );
    assert_eq!(
        risk("declare @n int = 0;\nraiserror('x', 0, 1) with nowait;\ndrop table t;"),
        Some(Risk::Drop)
    );
    assert_eq!(
        risk("declare @x int = 1;\nraiserror('x', 0, 1);\ndelete from t where id = @x;"),
        None
    );
    assert_eq!(risk("declare @x int = 1; select @x;"), None);
    // A `;` inside a text is not the end of a statement.
    assert_eq!(
        risk("declare @s varchar(20) = 'a; delete from t';\nraiserror(@s, 0, 1);"),
        None
    );
}

/// Agents: a batch with a DECLARE is still not one read nor one allowed write.
#[test]
fn agents_still_refuse_a_batch_with_variables() {
    let batch = "declare @x int = 1; select @x;";
    assert_eq!(SqlServerDialect.split_statements(batch).len(), 1);
    assert!(classify_read_only("sqlserver", batch).is_err());
    assert!(
        check_write(
            &SqlServerDialect,
            "declare @x int = 1; insert into t values (@x);"
        )
        .is_err()
    );
}

/// "Run the statement under the cursor" runs the whole batch from any line after the DECLARE.
#[test]
fn the_cursor_picks_the_whole_batch() {
    let script = "select 0;\ndeclare @id int = 5;\nselect @id;\nselect @id + 1;\nGO\nselect 9";
    let batch = "declare @id int = 5;\nselect @id;\nselect @id + 1;";
    for line in ["declare", "select @id;", "select @id + 1"] {
        let offset = script.find(line).unwrap() + 2;
        let span = statement_at(&SqlServerDialect, script, offset).unwrap();
        assert_eq!(span.text(script), batch, "{line}");
    }
    let first = statement_at(&SqlServerDialect, script, 2).unwrap();
    assert_eq!(first.text(script), "select 0");
}
