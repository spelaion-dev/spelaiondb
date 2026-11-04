//! Server process monitoring. SQL and process-control policy stay in the core.

use futures_util::StreamExt;
use spelunk_driver_api::Value;

use crate::{CoreEvent, Session, generate::scalar_count};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub id: String,
    pub user: String,
    pub host: String,
    pub database: Option<String>,
    pub command: String,
    pub seconds: Option<String>,
    pub state: Option<String>,
    pub sql: Option<String>,
    /// Engine-provided session/request start timestamps, for detecting recycled IDs.
    pub identity: Option<String>,
    /// One of `active`, `idle`, `waiting`, or `unknown`.
    pub activity: String,
}

pub struct ProcessSnapshot {
    pub own_id: String,
    pub processes: Vec<Process>,
    /// SQL Server rejected the server-wide views, so only the monitor is visible.
    pub limited: bool,
}

pub struct ProcessTarget {
    pub id: String,
    pub user: String,
    pub host: String,
    pub command: String,
    pub sql: Option<String>,
    pub identity: Option<String>,
}

fn queries(engine: &str) -> Result<(&'static str, &'static str), String> {
    match engine {
        "mysql" | "mariadb" => Ok(("SELECT CONNECTION_ID()", "SHOW FULL PROCESSLIST")),
        "postgres" => Ok(("SELECT pg_backend_pid()", POSTGRES_PROCESSES)),
        "sqlserver" => Ok(("SELECT @@SPID", SQLSERVER_PROCESSES)),
        _ => Err("process monitoring is not supported for this engine".into()),
    }
}

// Keep aliases uniform with SHOW FULL PROCESSLIST. Idle PostgreSQL sessions expose the
// previous query, which must not be presented as a currently running statement.
const POSTGRES_PROCESSES: &str = r#"
SELECT pid AS "Id", COALESCE(usename, '') AS "User",
       COALESCE(client_addr::text, client_hostname, '') AS "Host", datname AS "db",
       COALESCE(state, 'unknown') AS "Command",
       EXTRACT(EPOCH FROM (clock_timestamp() - COALESCE(CASE WHEN state = 'active' THEN query_start ELSE state_change END, backend_start)))::bigint AS "Time",
       CASE WHEN wait_event_type IS NULL THEN state ELSE wait_event_type || ': ' || COALESCE(wait_event, '') END AS "State",
       CASE WHEN state = 'active' THEN query ELSE NULL END AS "Info",
       backend_start::text || COALESCE('|' || CASE WHEN state = 'active' THEN query_start::text END, '') AS "Identity",
       CASE WHEN state = 'active' AND wait_event_type IS NOT NULL THEN 'waiting'
            WHEN state = 'active' THEN 'active' WHEN state LIKE 'idle%' THEN 'idle' ELSE 'unknown' END AS "Activity"
FROM pg_catalog.pg_stat_activity WHERE backend_type = 'client backend'"#;

// A session can have several requests (MARS); display its oldest active request. KILL is
// always a session-level operation. Internal server sessions are excluded.
const SQLSERVER_PROCESSES: &str = r#"
SELECT s.session_id AS [Id], COALESCE(s.login_name, '') AS [User],
       COALESCE(s.host_name, '') AS [Host], DB_NAME(COALESCE(r.database_id, s.database_id)) AS [db],
       COALESCE(r.command, 'sleep') AS [Command],
       CASE WHEN r.session_id IS NULL THEN DATEDIFF_BIG(second, s.last_request_end_time, GETDATE())
            ELSE CONVERT(bigint, r.total_elapsed_time) / 1000 END AS [Time],
       COALESCE(r.wait_type, r.status, s.status) AS [State], txt.text AS [Info],
       CONVERT(varchar(33), s.login_time, 126) + COALESCE('|' + CONVERT(varchar(33), r.start_time, 126), '') AS [Identity],
       CASE WHEN r.session_id IS NULL THEN 'idle' WHEN r.wait_type IS NOT NULL OR r.status = 'suspended' THEN 'waiting' ELSE 'active' END AS [Activity]
FROM sys.dm_exec_sessions AS s
OUTER APPLY (SELECT TOP (1) session_id, database_id, command, total_elapsed_time, wait_type, status, sql_handle, start_time
             FROM sys.dm_exec_requests WHERE session_id = s.session_id ORDER BY start_time) AS r
OUTER APPLY sys.dm_exec_sql_text(r.sql_handle) AS txt
WHERE s.is_user_process = 1"#;

// Without VIEW SERVER PERFORMANCE STATE / VIEW SERVER STATE, requests and SQL text are
// unavailable, but SQL Server still lets a login see its own session in this view.
const SQLSERVER_OWN_PROCESS: &str = r#"
SELECT s.session_id AS [Id], COALESCE(s.login_name, '') AS [User],
       COALESCE(s.host_name, '') AS [Host], DB_NAME(s.database_id) AS [db],
       s.status AS [Command], DATEDIFF_BIG(second, s.last_request_end_time, GETDATE()) AS [Time],
       s.status AS [State], CAST(NULL AS nvarchar(max)) AS [Info],
       CONVERT(varchar(33), s.login_time, 126) AS [Identity], 'unknown' AS [Activity]
FROM sys.dm_exec_sessions AS s WHERE s.session_id = @@SPID"#;

/// Fetch a server-wide snapshot without recording monitoring queries in query history.
pub async fn list_processes(session: &Session) -> Result<ProcessSnapshot, String> {
    let (own_sql, list_sql) = queries(session.dialect().name())?;
    let own_id = scalar_count(session, own_sql).await?.to_string();
    let (processes, limited) = match read_processes(session, list_sql).await {
        Ok(processes) => (processes, false),
        Err(original) if session.dialect().name() == "sqlserver" => {
            match read_processes(session, SQLSERVER_OWN_PROCESS).await {
                Ok(processes) => (processes, true),
                Err(_) => return Err(original),
            }
        }
        Err(error) => return Err(error),
    };
    Ok(ProcessSnapshot {
        own_id,
        processes,
        limited,
    })
}

async fn read_processes(session: &Session, sql: &str) -> Result<Vec<Process>, String> {
    let mut events = session.run_script(sql);
    let mut result = None;
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::ResultStarted { result: id, .. } => result = Some(id),
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            _ => {}
        }
    }
    let store = session.results();
    if let Some(error) = failure {
        if let Some(id) = result {
            store.remove(id);
        }
        return Err(error);
    }
    let id = result.ok_or("process list returned no result")?;
    let rows = (|| {
        let result = store.get(id).ok_or("process list is no longer available")?;
        let result = result.read().map_err(|_| "process list is unavailable")?;
        let schema = result.schema();
        let column = |name: &str| {
            schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
                .ok_or_else(|| format!("process list has no {name} column"))
        };
        let indices = [
            column("Id")?,
            column("User")?,
            column("Host")?,
            column("db")?,
            column("Command")?,
            column("Time")?,
            column("State")?,
            column("Info")?,
        ];
        let optional_column = |name: &str| {
            schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(name))
        };
        let identity_column = optional_column("Identity");
        let activity_column = optional_column("Activity");
        let mut processes = Vec::new();
        for batch in result.window(0, result.rows()) {
            for row in 0..batch.num_rows() {
                let value_at = |i: usize| -> Result<Option<String>, String> {
                    match Value::from_array(
                        batch.column(i).as_ref(),
                        row,
                        schema.columns[i].logical,
                    ) {
                        Some(Value::Null) => Ok(None),
                        Some(value) => Ok(Some(value.to_string())),
                        None => Err("invalid process list value".into()),
                    }
                };
                let value = |index: usize| value_at(indices[index]);
                let command = value(4)?.unwrap_or_default();
                let state = value(6)?;
                let activity = if let Some(i) = activity_column {
                    value_at(i)?.unwrap_or_else(|| "unknown".into())
                } else if command.eq_ignore_ascii_case("sleep") {
                    "idle".into()
                } else if state.as_deref().is_some_and(|s| {
                    s.to_ascii_lowercase().contains("waiting")
                        || s.to_ascii_lowercase().contains("locked")
                }) {
                    "waiting".into()
                } else {
                    "active".into()
                };
                processes.push(Process {
                    id: value(0)?.ok_or("process has no ID")?,
                    user: value(1)?.unwrap_or_default(),
                    host: value(2)?.unwrap_or_default(),
                    database: value(3)?,
                    command,
                    seconds: value(5)?,
                    state,
                    sql: value(7)?,
                    identity: identity_column.map(value_at).transpose()?.flatten(),
                    activity,
                });
            }
        }
        Ok::<_, String>(processes)
    })();
    store.remove(id);
    rows
}

fn action_sql(engine: &str, id: &str, terminate: bool) -> Result<String, String> {
    // Parse, then format the number; no SQL supplied by a caller can be executed here.
    let id = id.parse::<u64>().map_err(|_| "invalid process ID")?;
    if id == 0 {
        return Err("invalid process ID".into());
    }
    match (engine, terminate) {
        ("mysql" | "mariadb", terminate) => Ok(format!(
            "KILL {} {id}",
            if terminate { "CONNECTION" } else { "QUERY" }
        )),
        ("postgres", false) if id <= i32::MAX as u64 => {
            Ok(format!("SELECT pg_catalog.pg_cancel_backend({id})"))
        }
        ("postgres", true) if id <= i32::MAX as u64 => {
            Ok(format!("SELECT pg_catalog.pg_terminate_backend({id})"))
        }
        ("sqlserver", true) if id <= i16::MAX as u64 => Ok(format!("KILL {id}")),
        ("sqlserver", false) => Err(
            "SQL Server cannot cancel another session's query without ending the session".into(),
        ),
        _ => Err("invalid process ID or unsupported engine".into()),
    }
}

/// Recheck the target before sending a process-control command.
pub async fn control_process(
    session: &Session,
    target: &ProcessTarget,
    terminate: bool,
    read_only: bool,
    confirmed: bool,
) -> Result<(), String> {
    queries(session.dialect().name())?;
    if !confirmed {
        return Err("confirmation is required to stop a process".into());
    }
    if read_only {
        return Err("this connection is read-only".into());
    }
    let engine = session.dialect().name();
    let sql = action_sql(engine, &target.id, terminate)?;
    if matches!(engine, "postgres" | "sqlserver")
        && target.identity.as_deref().is_none_or(str::is_empty)
    {
        return Err("the server did not expose enough information to verify the process".into());
    }
    let snapshot = list_processes(session).await?;
    if target.id == snapshot.own_id {
        return Err("cannot stop the monitoring connection".into());
    }
    if !snapshot.processes.iter().any(|p| {
        p.id == target.id
            && p.user == target.user
            && p.host == target.host
            && p.command == target.command
            && p.sql == target.sql
            && p.identity == target.identity
    }) {
        return Err("the process has changed or is no longer visible; refresh the list".into());
    }
    let mut events = session.run_script(&sql);
    let mut result_id = None;
    let mut failure = None;
    while let Some(event) = events.next().await {
        match event {
            CoreEvent::StatementFailed { error, .. } => failure = Some(error.message),
            CoreEvent::ResultStarted { result, .. } => result_id = Some(result),
            _ => {}
        }
    }
    let result = result_id.and_then(|id| {
        let value = session.results().get(id).and_then(|stored| {
            let stored = stored.read().ok()?;
            let batch = stored.window(0, 1).into_iter().next()?;
            Value::from_array(
                batch.column(0).as_ref(),
                0,
                stored.schema().columns.first()?.logical,
            )
        });
        session.results().remove(id);
        value
    });
    if let Some(error) = failure {
        return Err(error);
    }
    if engine == "postgres" && result != Some(Value::Bool(true)) {
        return Err("the server did not signal the process; it may have already ended".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_ids_cannot_inject_sql() {
        assert_eq!(action_sql("mysql", "123", false).unwrap(), "KILL QUERY 123");
        assert_eq!(
            action_sql("mariadb", "123", true).unwrap(),
            "KILL CONNECTION 123"
        );
        assert_eq!(
            action_sql("postgres", "123", false).unwrap(),
            "SELECT pg_catalog.pg_cancel_backend(123)"
        );
        assert_eq!(
            action_sql("postgres", "123", true).unwrap(),
            "SELECT pg_catalog.pg_terminate_backend(123)"
        );
        assert_eq!(action_sql("sqlserver", "123", true).unwrap(), "KILL 123");
        assert!(action_sql("sqlserver", "123", false).is_err());
        assert!(action_sql("sqlserver", "32768", true).is_err());
        assert!(action_sql("postgres", "2147483648", true).is_err());
        for id in [
            "0",
            "",
            "-1",
            "1; DROP TABLE t",
            "1 OR 1=1",
            "18446744073709551616",
        ] {
            for engine in ["mysql", "postgres", "sqlserver"] {
                assert!(action_sql(engine, id, true).is_err(), "{engine}: {id}");
            }
        }
    }
}
