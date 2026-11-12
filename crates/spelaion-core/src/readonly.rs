//! Read-only execution with limits (ADR-008, layers 3 to 5; ADR-020 for the other engines).
//! Used by the MCP.
//!
//! Layers, from the outside in:
//! 1. Classifier ([`classify_read_only`]): only recognised reads, nothing that reaches
//!    outside the database.
//! 2. The engine's own barrier, always ending in `ROLLBACK`:
//!    - PostgreSQL: `BEGIN READ ONLY` with `statement_timeout`;
//!    - SQLite: a connection opened read-only plus `PRAGMA query_only`;
//!    - MySQL and MariaDB: `START TRANSACTION READ ONLY`, after making the session read
//!      strings as the classifier does ([`Session::mysql_default_quoting`]);
//!    - SQL Server: a transaction that is always rolled back (it has no read-only one);
//!      `EXPLAIN` becomes `SET SHOWPLAN_XML ON`, under which nothing runs;
//!    - DuckDB: `enable_external_access = false` (no files, network nor extensions; it
//!      cannot be turned back on) and `BEGIN TRANSACTION READ ONLY`, on a database opened
//!      read-only unless the connection lets agents write.
//! 3. Limits: maximum time and maximum number of rows.
//!
//! None of them replaces a read-only database user, which is the only real guarantee
//! (a function with side effects looks like a read).

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use spelaion_driver_api::{ColumnMeta, DriverError, ExecEvent, ExecOptions, Value};

use crate::Session;
use crate::classify::{ReadKind, classify_read_only, reject_mysql_executable_comments};

#[derive(Debug, Clone)]
pub struct ReadLimits {
    pub max_rows: usize,
    pub timeout: Duration,
}

impl Default for ReadLimits {
    fn default() -> Self {
        Self {
            max_rows: 200,
            timeout: Duration::from_secs(30),
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReadResult {
    pub columns: Vec<ColumnMeta>,
    pub rows: Vec<Vec<Value>>,
    /// There were more rows than `max_rows`.
    pub truncated: bool,
    pub kind: ReadKind,
    pub elapsed: Duration,
    /// Positions of the columns the connection masks (ADR-051): their values in `rows` are
    /// the mask (NULL stays NULL).
    pub masked: Vec<usize>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The classifier rejected it: nothing was sent to the server.
    #[error("rejected: {0}")]
    Rejected(String),
    #[error("exceeded the time limit of {} s", .0.as_secs())]
    Timeout(Duration),
    #[error("{0}")]
    Database(#[from] DriverError),
    #[error("read-only mode is not available for {0}")]
    Unsupported(String),
}

/// DuckDB without files, network nor extensions, for agents. DuckDB refuses to turn it back on
/// while the database is open, so a statement cannot undo it.
pub(crate) const DUCKDB_NO_EXTERNAL_ACCESS: &str = "set enable_external_access = false";

/// Name of the server-side cursor used in PostgreSQL.
const CURSOR: &str = "spelaion_readonly";

/// Margin of the client timer over `statement_timeout` in PostgreSQL.
const CLIENT_GRACE: Duration = Duration::from_secs(2);

impl Session {
    /// Runs a single read statement with limits. It never leaves a transaction open.
    pub async fn run_read_only(
        &self,
        sql: &str,
        limits: &ReadLimits,
    ) -> Result<ReadResult, ReadError> {
        let engine = self.dialect().name();
        let sql = sql.trim().trim_end_matches(';').trim();
        reject_mysql_executable_comments(engine, sql).map_err(ReadError::Rejected)?;
        let kind = classify_read_only(engine, sql).map_err(ReadError::Rejected)?;
        let started = Instant::now();

        let run = async {
            match engine {
                "postgres" => self.read_postgres(sql, kind, limits).await,
                "sqlite" => {
                    // Redundant if the connection was opened read-only, on purpose.
                    self.drain("pragma query_only = on").await?;
                    self.collect(sql, limits.max_rows).await
                }
                "mysql" => {
                    self.mysql_default_quoting()
                        .await
                        .map_err(ReadError::Rejected)?;
                    self.rolled_back("start transaction read only", sql, limits)
                        .await
                }
                "sqlserver" => match kind {
                    ReadKind::Explain => self.showplan(sql, limits).await,
                    ReadKind::Query | ReadKind::Show => {
                        self.rolled_back("begin transaction", sql, limits).await
                    }
                },
                "duckdb" => {
                    self.drain(DUCKDB_NO_EXTERNAL_ACCESS).await?;
                    self.rolled_back("begin transaction read only", sql, limits)
                        .await
                }
                other => Err(ReadError::Unsupported(other.into())),
            }
        };
        // In PostgreSQL `statement_timeout` (server side) leads: its error takes the normal
        // path, which does ROLLBACK without races. The client timer, with a margin, is only
        // the last resort (a hung network). If they were equal, the client cancellation
        // could arrive late and cancel the ROLLBACK itself, leaving the transaction open.
        let client_timeout = match engine {
            "postgres" => limits.timeout + CLIENT_GRACE,
            _ => limits.timeout,
        };
        let (columns, mut rows, truncated) = match tokio::time::timeout(client_timeout, run).await {
            Ok(result) => result?,
            Err(_) => {
                let _ = self.cancel().await;
                if engine != "sqlite" {
                    self.rollback_until_clean().await;
                }
                return Err(ReadError::Timeout(limits.timeout));
            }
        };
        // Agents never get a masked column's values (ADR-051). A plan shows the plan, not rows.
        let masked = match kind {
            ReadKind::Explain => Vec::new(),
            ReadKind::Query | ReadKind::Show => self.masking().mask_rows(&columns, &mut rows),
        };
        Ok(ReadResult {
            columns,
            rows,
            truncated,
            kind,
            elapsed: started.elapsed(),
            masked,
        })
    }

    async fn read_postgres(
        &self,
        sql: &str,
        kind: ReadKind,
        limits: &ReadLimits,
    ) -> Result<(Vec<ColumnMeta>, Vec<Vec<Value>>, bool), ReadError> {
        self.drain("begin read only").await?;
        let result = async {
            let timeout_ms = limits.timeout.as_millis().min(i32::MAX as u128);
            self.drain(&format!("set local statement_timeout = {timeout_ms}"))
                .await?;
            match kind {
                // With a cursor only the rows that fit travel (plus one, to know if there are more).
                ReadKind::Query => {
                    self.drain(&format!("declare {CURSOR} no scroll cursor for {sql}"))
                        .await?;
                    self.collect(
                        &format!("fetch {} from {CURSOR}", limits.max_rows + 1),
                        limits.max_rows,
                    )
                    .await
                }
                ReadKind::Explain | ReadKind::Show => self.collect(sql, limits.max_rows).await,
            }
        }
        .await;
        // Always ROLLBACK: also after an error, which leaves the transaction aborted.
        let rollback = self.drain("rollback").await;
        let result = result?;
        rollback?;
        Ok(result)
    }

    /// Runs `sql` inside the transaction `begin` opens, and always rolls it back.
    async fn rolled_back(
        &self,
        begin: &str,
        sql: &str,
        limits: &ReadLimits,
    ) -> Result<(Vec<ColumnMeta>, Vec<Vec<Value>>, bool), ReadError> {
        self.drain(begin).await?;
        let result = self.collect(sql, limits.max_rows).await;
        let rollback = self.drain(self.rollback_statement()).await;
        let result = result?;
        rollback?;
        Ok(result)
    }

    /// SQL Server's plan: under `SHOWPLAN_XML` statements are compiled, never run. Each SET
    /// goes in a batch of its own, as SQL Server requires.
    async fn showplan(
        &self,
        sql: &str,
        limits: &ReadLimits,
    ) -> Result<(Vec<ColumnMeta>, Vec<Vec<Value>>, bool), ReadError> {
        let query = sql
            .get(..7)
            .filter(|word| word.eq_ignore_ascii_case("explain"))
            .map(|_| sql[7..].trim())
            .ok_or_else(|| ReadError::Rejected("EXPLAIN must come first".into()))?;
        // One driver request owns ON, the compiled query, and OFF. In particular, a timeout
        // dropping this future cannot skip OFF or let another actor request run in between.
        let events = self
            .connection()
            .execute_plan(query, spelaion_driver_api::PlanMode::Estimated)
            .await?;
        self.collect_events(events, limits.max_rows).await
    }

    /// MySQL and MariaDB: makes the session read strings and double quotes as the agents'
    /// checks do (SDB-116). They parse MySQL with its default `sql_mode`, where `\` escapes a
    /// quote and `"` starts a string. Under `NO_BACKSLASH_ESCAPES` or `ANSI_QUOTES` the server
    /// would end a string elsewhere, and text the checks saw inside a string (a `WHERE` hidden
    /// behind `-- `, a refused function) would run. So, on every call, before the statement and
    /// on the same session: the session's `sql_mode` is read, and if it has either mode it is
    /// set to the same list without them, then read again. `Err` names the mode that could not
    /// be taken away: the statement is then refused, never run.
    pub(crate) async fn mysql_default_quoting(&self) -> Result<(), String> {
        const LOOSE: [&str; 2] = ["NO_BACKSLASH_ESCAPES", "ANSI_QUOTES"];
        let read_mode = || async {
            let (_, rows, _) = self
                .collect("select @@session.sql_mode", 1)
                .await
                .map_err(|e| format!("could not read the session's sql_mode: {e}"))?;
            let mode = match rows.first().and_then(|row| row.first()) {
                Some(Value::Text(text)) => text.clone(),
                Some(Value::Null) | None => String::new(),
                Some(other) => other.to_string(),
            };
            Ok::<_, String>(
                mode.split(',')
                    .map(|m| m.trim().to_ascii_uppercase())
                    .filter(|m| !m.is_empty())
                    .collect::<Vec<_>>(),
            )
        };
        let modes = read_mode().await?;
        let loose: Vec<&String> = modes
            .iter()
            .filter(|m| LOOSE.contains(&m.as_str()))
            .collect();
        if loose.is_empty() {
            return Ok(());
        }
        let named = loose
            .iter()
            .map(|m| m.as_str())
            .collect::<Vec<_>>()
            .join(" and ");
        let kept: Vec<&str> = modes
            .iter()
            .map(String::as_str)
            .filter(|m| !LOOSE.contains(m))
            .collect();
        // Mode names are words of letters, digits and `_`; anything else is not written into
        // the statement.
        if kept
            .iter()
            .any(|m| !m.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            return Err(format!(
                "the session's sql_mode has {named}, and its other modes could not be kept"
            ));
        }
        self.drain(&format!("set session sql_mode = '{}'", kept.join(",")))
            .await
            .map_err(|e| {
                format!("the session's sql_mode has {named}, and it could not be turned off: {e}")
            })?;
        let now = read_mode().await?;
        if let Some(still) = now.iter().find(|m| LOOSE.contains(&m.as_str())) {
            return Err(format!(
                "the session's sql_mode has {still}, and it could not be turned off"
            ));
        }
        Ok(())
    }

    /// The ROLLBACK that is harmless outside a transaction, in this engine.
    pub(crate) fn rollback_statement(&self) -> &'static str {
        match self.dialect().name() {
            "sqlserver" => "if @@trancount > 0 rollback",
            _ => "rollback",
        }
    }

    /// After a timeout cancellation: ROLLBACK until it works. Cancellation is asynchronous and
    /// may hit the ROLLBACK itself; a ROLLBACK outside a transaction is harmless.
    pub(crate) async fn rollback_until_clean(&self) {
        for _ in 0..5 {
            if self.drain(self.rollback_statement()).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Runs and discards the rows (control statements).
    async fn drain(&self, sql: &str) -> Result<(), ReadError> {
        let mut events = self
            .connection()
            .execute(sql, &ExecOptions::default())
            .await?;
        while let Some(event) = events.next().await {
            event?;
        }
        Ok(())
    }

    /// Runs and keeps at most `max_rows` rows. Stops reading when the limit is reached.
    async fn collect(
        &self,
        sql: &str,
        max_rows: usize,
    ) -> Result<(Vec<ColumnMeta>, Vec<Vec<Value>>, bool), ReadError> {
        let options = ExecOptions {
            batch_size: (max_rows + 1).clamp(1, 10_000),
            // Masking matches each column's origin too (ADR-052).
            origins: !self.masking().is_empty(),
        };
        let events = self.connection().execute(sql, &options).await?;
        self.collect_events(events, max_rows).await
    }

    async fn collect_events(
        &self,
        mut events: spelaion_driver_api::EventStream,
        max_rows: usize,
    ) -> Result<(Vec<ColumnMeta>, Vec<Vec<Value>>, bool), ReadError> {
        let mut columns = Vec::new();
        let mut rows = Vec::new();
        let mut truncated = false;
        while let Some(event) = events.next().await {
            match event? {
                ExecEvent::Columns(schema) => columns = schema.columns.clone(),
                ExecEvent::Batch(batch) => {
                    for r in 0..batch.num_rows() {
                        if rows.len() == max_rows {
                            truncated = true;
                            break;
                        }
                        let row = columns
                            .iter()
                            .enumerate()
                            .map(|(c, meta)| {
                                Value::from_array(batch.column(c).as_ref(), r, meta.logical)
                                    .unwrap_or(Value::Null)
                            })
                            .collect();
                        rows.push(row);
                    }
                    if truncated {
                        break;
                    }
                }
                ExecEvent::Notice(_) | ExecEvent::Done { .. } => {}
            }
        }
        Ok((columns, rows, truncated))
    }
}
