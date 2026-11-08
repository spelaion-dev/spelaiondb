use super::*;
use crate::{CoreEvent, HistoryScope, OpenConnection, QueryHistory, ScriptOptions, Session};
use futures_util::StreamExt;
use spelaion_config::Safeguards;
use sqlparser::ast::Statement;
use sqlparser::parser::Parser;

/// Own internal Arrow handles even if the caller drops the execution future early.
struct PlanResults {
    store: std::sync::Arc<crate::ResultStore>,
    ids: Vec<crate::ResultId>,
}
impl Drop for PlanResults {
    fn drop(&mut self) {
        for id in &self.ids {
            self.store.remove(*id);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct ExplainOptions {
    pub analyze: bool,
    pub buffers: bool,
    /// The separate warning that ANALYZE executes the query was accepted.
    pub analyze_confirmed: bool,
    /// The existing statement/type guard was accepted.
    pub confirmed: bool,
    /// This tab's override, as for ordinary scripts: None uses the connection, zero disables.
    pub timeout_seconds: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "bindings", derive(specta::Type))]
#[serde(rename_all = "camelCase")]
pub struct PlanCapabilities {
    pub estimated: bool,
    pub analyze: bool,
    pub buffers: bool,
    pub note: String,
}

pub fn plan_capabilities(session: &Session, policy: &ExplainPolicy) -> PlanCapabilities {
    let engine = session.dialect().name();
    let supported = matches!(engine, "postgres" | "mysql");
    // MySQL and MariaDB share a dialect but not the ANALYZE syntax: it needs the flavor of
    // the connection that was actually opened.
    let analyzable = engine == "postgres" || policy.flavor.is_some();
    PlanCapabilities {
        estimated: supported,
        analyze: supported && analyzable && !policy.read_only,
        buffers: engine == "postgres" && !policy.read_only,
        note: if !supported {
            format!(
                "Normalized execution plans are not available for {} yet.",
                session.dialect().name()
            )
        } else if policy.read_only {
            "Read-only mode permits estimated read plans only. ANALYZE executes the query and is refused by the read-only barrier.".into()
        } else {
            "ANALYZE executes the statement on this tab's current session and transaction. The current auto-commit/manual-commit mode applies; the plan viewer adds no commit or rollback.".into()
        },
    }
}

/// Trusted policy captured from the actual open connection, never supplied by React.
#[derive(Debug, Clone)]
pub struct ExplainPolicy {
    read_only: bool,
    safeguards: Safeguards,
    timeout: Option<std::time::Duration>,
    flavor: Option<Flavor>,
}
impl ExplainPolicy {
    pub fn for_connection(open: &OpenConnection) -> Self {
        use spelaion_config::Engine;
        Self {
            read_only: open.resolved.params.read_only,
            safeguards: open.resolved.safeguards(),
            timeout: open.resolved.statement_timeout,
            // The driver was opened for this engine. If the server is the other flavor, its
            // ANALYZE syntax is a syntax error there, refused before anything runs.
            flavor: match open.resolved.engine {
                Engine::Mysql => Some(Flavor::Mysql),
                Engine::Mariadb => Some(Flavor::Mariadb),
                Engine::Postgres | Engine::Sqlite | Engine::Duckdb | Engine::Sqlserver => None,
            },
        }
    }
    /// For trusted library adapters that own a Session directly. Without the connection's
    /// engine, MySQL and MariaDB plans are estimated only.
    pub fn new(read_only: bool, safeguards: Safeguards) -> Self {
        Self {
            read_only,
            safeguards,
            timeout: None,
            flavor: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlanPreflight {
    pub sql: String,
    pub risks: Vec<crate::RiskyStatement>,
    pub executes_query: bool,
}

/// Cannot be constructed outside this module. Execution SQL and transaction intent always
/// come from the same parsed request; an adapter cannot claim a write is a read.
pub(crate) struct PreparedPlan {
    sql: String,
    intent: String,
}
impl PreparedPlan {
    pub(crate) fn sql(&self) -> &str {
        &self.sql
    }
    pub(crate) fn intent(&self) -> &str {
        &self.intent
    }
}

fn prepare(
    session: &Session,
    sql: &str,
    options: ExplainOptions,
    policy: &ExplainPolicy,
) -> Result<PreparedPlan, PlanError> {
    let engine = session.dialect().name();
    if !matches!(engine, "postgres" | "mysql") {
        return Err(PlanError::Unsupported(engine.into()));
    }
    if sql.len() > 1024 * 1024 {
        return Err(PlanError::Limit("query text is larger than 1 MiB".into()));
    }
    if session.dialect().split_statements(sql).len() != 1 {
        return Err(PlanError::Invalid(
            "explain exactly one SELECT, VALUES, INSERT, UPDATE, DELETE or MERGE statement".into(),
        ));
    }
    let parsed = Parser::parse_sql(crate::guard::sql_dialect(engine).as_ref(), sql)
        .map_err(|e| PlanError::Invalid(e.to_string()))?;
    if !matches!(
        parsed.as_slice(),
        [Statement::Query(_)
            | Statement::Insert { .. }
            | Statement::Update { .. }
            | Statement::Delete { .. }
            | Statement::Merge { .. }]
    ) {
        return Err(PlanError::Invalid("this statement cannot be explained; choose one SELECT, VALUES, INSERT, UPDATE, DELETE or MERGE".into()));
    }
    if options.buffers && engine != "postgres" {
        return Err(PlanError::Invalid(
            "BUFFERS is only available for PostgreSQL plans".into(),
        ));
    }
    if options.buffers && !options.analyze {
        return Err(PlanError::Invalid(
            "enable ANALYZE before requesting execution buffers".into(),
        ));
    }
    if options.analyze && policy.read_only {
        return Err(PlanError::Execution(
            "this connection is read-only: ANALYZE executes the query".into(),
        ));
    }
    let intent = if options.analyze {
        sql.trim().to_owned()
    } else {
        "SELECT 1".into()
    };
    let sql = match (engine, options.analyze, policy.flavor) {
        ("postgres", analyze, _) => format!(
            "EXPLAIN (FORMAT JSON, VERBOSE TRUE, ANALYZE {analyze}, BUFFERS {})\n{}",
            options.buffers,
            sql.trim()
        ),
        (_, false, _) => format!("EXPLAIN FORMAT=JSON\n{}", sql.trim()),
        // Each flavor's ANALYZE is a syntax error on the other one, so a connection opened
        // as the wrong engine is refused by the server before the statement runs.
        (_, true, Some(Flavor::Mysql)) => format!("EXPLAIN ANALYZE FORMAT=TREE\n{}", sql.trim()),
        (_, true, Some(Flavor::Mariadb)) => format!("ANALYZE FORMAT=JSON\n{}", sql.trim()),
        (_, true, None) => {
            return Err(PlanError::Invalid(
                "ANALYZE needs the connection's engine (MySQL or MariaDB)".into(),
            ));
        }
    };
    if policy.read_only {
        // The PostgreSQL driver's read_only flag does not establish a server barrier.
        // Preserve the established classifier: estimated read plans only, no ANALYZE.
        crate::classify_read_only(engine, &sql)
            .map_err(|e| PlanError::Execution(format!("this connection is read-only: {e}")))?;
    }
    Ok(PreparedPlan { sql, intent })
}

pub fn explain_preflight(
    session: &Session,
    sql: &str,
    options: ExplainOptions,
    policy: &ExplainPolicy,
) -> Result<PlanPreflight, PlanError> {
    let prepared = prepare(session, sql, options, policy)?;
    let mut risks = crate::script_risks(
        session.dialect().as_ref(),
        &prepared.intent,
        &policy.safeguards,
    );
    for risk in &mut risks {
        risk.span = spelaion_driver_api::StatementSpan {
            start: 0,
            end: sql.len(),
        };
    }
    Ok(PlanPreflight {
        sql: prepared.sql,
        risks,
        executes_query: options.analyze,
    })
}

/// Estimated read plans for agents retain every existing classifier, native read-only
/// transaction and timeout barrier. This entry point cannot request ANALYZE. The open
/// connection only names the engine (MySQL or MariaDB) that wrote the plan.
pub async fn explain_read_only(
    open: &OpenConnection,
    sql: &str,
    limits: &crate::ReadLimits,
) -> Result<PlanDocument, PlanError> {
    let session = &open.session;
    let flavor = ExplainPolicy::for_connection(open).flavor;
    let prepared = prepare(
        session,
        sql,
        ExplainOptions::default(),
        &ExplainPolicy::new(true, Safeguards::default()),
    )?;
    let result = session
        .run_read_only(prepared.sql(), limits)
        .await
        .map_err(|e| PlanError::Execution(e.to_string()))?;
    if result.truncated || result.rows.len() != 1 || result.columns.len() != 1 {
        return Err(PlanError::Invalid(
            "EXPLAIN did not return one complete JSON cell".into(),
        ));
    }
    let raw = result.rows[0][0].to_string();
    parse_plan_from(&raw, "Execution plan", flavor)
}

/// An explicit user run on the source session: serialized, cancellable, transactional and
/// recorded in history. Never reconnects or retries. Internal Arrow handles are released.
pub async fn explain(
    session: &Session,
    sql: &str,
    options: ExplainOptions,
    policy: &ExplainPolicy,
    history: Option<(QueryHistory, HistoryScope)>,
    mut observer: impl FnMut(CoreEvent),
) -> Result<PlanDocument, PlanError> {
    let prepared = prepare(session, sql, options, policy)?;
    if options.analyze && !options.analyze_confirmed {
        return Err(PlanError::Execution(
            "ANALYZE executes the query; accept its execution warning first".into(),
        ));
    }
    crate::check_script(
        session.dialect().as_ref(),
        &prepared.intent,
        &policy.safeguards,
        options.confirmed,
    )
    .map_err(|e| PlanError::Execution(e.to_string()))?;
    let events = session.run_plan(
        &prepared,
        ScriptOptions {
            statement_timeout: match options.timeout_seconds {
                None => policy.timeout,
                Some(0) => None,
                Some(seconds) => Some(std::time::Duration::from_secs(seconds.into())),
            },
            ..Default::default()
        },
    );
    let mut events = match history {
        Some((history, scope)) => history.observe_run(session, prepared.sql(), scope, None, events),
        None => events,
    };
    let mut results = PlanResults {
        store: session.results().clone(),
        ids: Vec::new(),
    };
    let mut error = None;
    let mut cancelled = false;
    while let Some(event) = events.next().await {
        match &event {
            CoreEvent::ResultStarted { result, .. } => results.ids.push(*result),
            CoreEvent::Rows { total_rows, .. } if *total_rows > 1 => {
                error = Some("EXPLAIN returned more than one JSON row".into());
                let _ = session.cancel().await;
            }
            CoreEvent::StatementFailed { error: failure, .. } => {
                error = Some(failure.message.clone())
            }
            CoreEvent::ScriptFinished { summary, .. } => {
                cancelled = summary.executed == 0 || summary.succeeded == 0
            }
            CoreEvent::StatementStarted { .. }
            | CoreEvent::Rows { .. }
            | CoreEvent::Notice { .. }
            | CoreEvent::StatementFinished { .. }
            | CoreEvent::ErrorPaused { .. }
            | CoreEvent::HistoryWarning { .. } => {}
        }
        // Plans own their data; do not publish Arrow handles as query-result tabs.
        if !matches!(
            event,
            CoreEvent::ResultStarted { .. } | CoreEvent::Rows { .. }
        ) {
            observer(event);
        }
    }
    (|| {
        if let Some(error) = error {
            return Err(PlanError::Execution(error));
        }
        if cancelled {
            return Err(PlanError::Execution("Query cancelled".into()));
        }
        if results.ids.len() != 1 {
            return Err(PlanError::Invalid(
                "EXPLAIN did not return one result".into(),
            ));
        }
        let result = session
            .results()
            .get(results.ids[0])
            .ok_or_else(|| PlanError::Invalid("EXPLAIN result was released".into()))?;
        let result = result.read().expect("result lock poisoned");
        if result.rows() != 1 || result.schema().columns.len() != 1 {
            return Err(PlanError::Invalid(
                "EXPLAIN did not return one JSON cell".into(),
            ));
        }
        let raw = result
            .value_at(0, 0)
            .ok_or_else(|| PlanError::Invalid("missing EXPLAIN JSON".into()))?
            .to_string();
        parse_plan_from(&raw, "Execution plan", policy.flavor)
    })()
}
