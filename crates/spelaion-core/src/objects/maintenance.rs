//! Maintenance actions of the explorer (SDB-392): VACUUM, ANALYZE, refreshing a materialized
//! view, TRUNCATE with its options and triggers on and off on PostgreSQL; CHECK, REPAIR,
//! ANALYZE, OPTIMIZE and TRUNCATE on MySQL and MariaDB; rebuilding a table or its indexes and
//! triggers on and off on SQL Server; the integrity check and VACUUM on SQLite, VACUUM and
//! CHECKPOINT on DuckDB.
//!
//! Each is reviewed SQL written here (ADR-026): the app shows the statement with its options,
//! and runs it as shown in an editor tab of its own, so the guard asks what it asks of any run
//! (a TRUNCATE), a read-only connection refuses it (ADR-053), its output (notices, the rows a
//! check returns) lands in that tab's results and Messages, and VACUUM never runs inside a
//! transaction: a new tab has none open, and the core never opens one before it (ADR-031).
//! Names are quoted by the engine's dialect, options are the closed lists below: the UI names
//! a table and ticks options, it never writes SQL.

use spelaion_driver_api::{Dialect, ObjectKind, Value};

use super::{ObjectError, ObjectTarget, qualified_name};
use crate::Session;
use crate::export::sql_literal;
use crate::generate::scalar_metadata;

/// An action of the explorer's Maintenance menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaintenanceKind {
    Vacuum,
    Analyze,
    RefreshMaterializedView,
    Truncate,
    EnableTriggers,
    DisableTriggers,
    Check,
    Repair,
    Optimize,
    RebuildTable,
    RebuildIndexes,
    IntegrityCheck,
    Checkpoint,
}

/// PostgreSQL's `VACUUM (INDEX_CLEANUP …)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexCleanup {
    Auto,
    On,
    Off,
}

/// MySQL's and MariaDB's `CHECK TABLE` modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckMode {
    Quick,
    Fast,
    Medium,
    Extended,
    Changed,
}

/// The options of an action; each action reads only its own.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaintenanceOptions {
    /// VACUUM (PostgreSQL).
    pub full: bool,
    pub freeze: bool,
    pub analyze: bool,
    pub disable_page_skipping: bool,
    pub skip_locked: bool,
    pub index_cleanup: Option<IndexCleanup>,
    /// `TRUNCATE false` keeps the empty pages at the end of the table.
    pub truncate_pages: Option<bool>,
    /// VACUUM and ANALYZE (PostgreSQL): the server reports what it did, as notices.
    pub verbose: bool,
    /// REFRESH MATERIALIZED VIEW: `WITH NO DATA` empties it until the next refresh.
    pub no_data: bool,
    /// REFRESH MATERIALIZED VIEW CONCURRENTLY: readers keep the old rows meanwhile.
    pub concurrently: bool,
    /// TRUNCATE (PostgreSQL): `ONLY` leaves the descendants of a parent table.
    pub only: bool,
    pub restart_identity: bool,
    /// TRUNCATE (PostgreSQL): also empties the tables whose foreign keys point to it.
    pub cascade: bool,
    /// CHECK TABLE (MySQL, MariaDB).
    pub check_mode: Option<CheckMode>,
    /// REPAIR TABLE (MySQL, MariaDB).
    pub quick: bool,
    pub extended: bool,
    pub use_frm: bool,
    /// ANALYZE, OPTIMIZE and REPAIR (MySQL, MariaDB): `NO_WRITE_TO_BINLOG`, not replicated.
    pub local: bool,
}

/// An action to plan: what it acts on, which one, and its options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenanceRequest {
    /// A relation, or the schema (database file) for SQLite's and DuckDB's actions.
    pub target: ObjectTarget,
    pub kind: MaintenanceKind,
    pub options: MaintenanceOptions,
}

/// What the app shows before it runs an action.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaintenancePlan {
    /// The statement, as it runs, ending in `;`.
    pub sql: String,
    /// The engine refuses it inside a transaction (VACUUM): it runs in a tab of its own.
    pub outside_transaction: bool,
    /// What a TRUNCATE … CASCADE empties besides the table, qualified, nearest first.
    pub also_empties: Vec<String>,
}

fn is_table(kind: ObjectKind) -> bool {
    matches!(kind, ObjectKind::Table | ObjectKind::PartitionedTable)
}

/// The actions the explorer offers on `target` on this engine, in menu order.
pub fn maintenance_kinds(dialect: &dyn Dialect, target: &ObjectTarget) -> Vec<MaintenanceKind> {
    use MaintenanceKind::*;
    if !dialect.is_sql() {
        return Vec::new();
    }
    match (dialect.name(), target) {
        ("postgres", ObjectTarget::Relation { kind, .. }) if is_table(*kind) => {
            vec![Vacuum, Analyze, Truncate, EnableTriggers, DisableTriggers]
        }
        ("postgres", ObjectTarget::Relation { kind, .. })
            if *kind == ObjectKind::MaterializedView =>
        {
            vec![RefreshMaterializedView, Vacuum, Analyze]
        }
        ("mysql", ObjectTarget::Relation { kind, .. }) if is_table(*kind) => {
            vec![Check, Analyze, Optimize, Repair, Truncate]
        }
        ("sqlserver", ObjectTarget::Relation { kind, .. }) if is_table(*kind) => {
            vec![
                RebuildTable,
                RebuildIndexes,
                EnableTriggers,
                DisableTriggers,
            ]
        }
        ("sqlite", ObjectTarget::Schema { .. }) => vec![IntegrityCheck, Vacuum],
        ("duckdb", ObjectTarget::Schema { .. }) => vec![Vacuum, Checkpoint],
        _ => Vec::new(),
    }
}

fn unsupported() -> ObjectError {
    ObjectError::Unsupported("this action is not offered for this object on this engine".into())
}

/// The statement of an action, without its final `;`, checked against what the engine offers
/// and against options that cannot go together.
pub fn maintenance_statement(
    dialect: &dyn Dialect,
    request: &MaintenanceRequest,
) -> Result<String, ObjectError> {
    use MaintenanceKind::*;
    if !maintenance_kinds(dialect, &request.target).contains(&request.kind) {
        return Err(unsupported());
    }
    let o = &request.options;
    let engine = dialect.name();
    let relation = match &request.target {
        ObjectTarget::Relation { schema, name, .. } => {
            Some(qualified_name(dialect, schema, std::slice::from_ref(name)))
        }
        _ => None,
    };
    let table = || relation.clone().ok_or_else(unsupported);
    let schema = match &request.target {
        ObjectTarget::Schema { name } => Some(dialect.quote_identifier(name)),
        _ => None,
    };
    let local = if o.local { "NO_WRITE_TO_BINLOG " } else { "" };
    Ok(match (engine, request.kind) {
        ("postgres", Vacuum) => {
            if o.full && o.disable_page_skipping {
                return Err(ObjectError::Unsupported(
                    "VACUUM FULL cannot be combined with DISABLE_PAGE_SKIPPING".into(),
                ));
            }
            let mut options = Vec::new();
            if o.full {
                options.push("FULL".to_string());
            }
            if o.freeze {
                options.push("FREEZE".into());
            }
            if o.verbose {
                options.push("VERBOSE".into());
            }
            if o.analyze {
                options.push("ANALYZE".into());
            }
            if o.disable_page_skipping {
                options.push("DISABLE_PAGE_SKIPPING".into());
            }
            if o.skip_locked {
                options.push("SKIP_LOCKED".into());
            }
            if let Some(cleanup) = o.index_cleanup {
                options.push(format!(
                    "INDEX_CLEANUP {}",
                    match cleanup {
                        IndexCleanup::Auto => "AUTO",
                        IndexCleanup::On => "ON",
                        IndexCleanup::Off => "OFF",
                    }
                ));
            }
            if let Some(truncate) = o.truncate_pages {
                options.push(format!("TRUNCATE {truncate}"));
            }
            if options.is_empty() {
                format!("VACUUM {}", table()?)
            } else {
                format!("VACUUM ({}) {}", options.join(", "), table()?)
            }
        }
        ("postgres", Analyze) => {
            let verbose = if o.verbose { " VERBOSE" } else { "" };
            format!("ANALYZE{verbose} {}", table()?)
        }
        ("postgres", RefreshMaterializedView) => {
            if o.concurrently && o.no_data {
                return Err(ObjectError::Unsupported(
                    "CONCURRENTLY refreshes the rows readers see: it cannot leave the view without data".into(),
                ));
            }
            format!(
                "REFRESH MATERIALIZED VIEW {}{}{}",
                if o.concurrently { "CONCURRENTLY " } else { "" },
                table()?,
                if o.no_data { " WITH NO DATA" } else { "" }
            )
        }
        ("postgres", Truncate) => format!(
            "TRUNCATE {}{}{}{}",
            if o.only { "ONLY " } else { "" },
            table()?,
            if o.restart_identity {
                " RESTART IDENTITY"
            } else {
                ""
            },
            if o.cascade { " CASCADE" } else { "" }
        ),
        // `USER`: the table's own triggers. `ALL` would also stop the foreign keys' triggers,
        // which only a superuser may do, and which leaves the keys unchecked.
        ("postgres", EnableTriggers) => format!("ALTER TABLE {} ENABLE TRIGGER USER", table()?),
        ("postgres", DisableTriggers) => format!("ALTER TABLE {} DISABLE TRIGGER USER", table()?),
        ("mysql", Check) => {
            let mode = match o.check_mode {
                None => "",
                Some(CheckMode::Quick) => " QUICK",
                Some(CheckMode::Fast) => " FAST",
                Some(CheckMode::Medium) => " MEDIUM",
                Some(CheckMode::Extended) => " EXTENDED",
                Some(CheckMode::Changed) => " CHANGED",
            };
            format!("CHECK TABLE {}{mode}", table()?)
        }
        ("mysql", Repair) => format!(
            "REPAIR {local}TABLE {}{}{}{}",
            table()?,
            if o.quick { " QUICK" } else { "" },
            if o.extended { " EXTENDED" } else { "" },
            if o.use_frm { " USE_FRM" } else { "" }
        ),
        ("mysql", Analyze) => format!("ANALYZE {local}TABLE {}", table()?),
        ("mysql", Optimize) => format!("OPTIMIZE {local}TABLE {}", table()?),
        ("mysql", Truncate) => format!("TRUNCATE TABLE {}", table()?),
        ("sqlserver", RebuildTable) => format!("ALTER TABLE {} REBUILD", table()?),
        ("sqlserver", RebuildIndexes) => format!("ALTER INDEX ALL ON {} REBUILD", table()?),
        ("sqlserver", EnableTriggers) => format!("ENABLE TRIGGER ALL ON {}", table()?),
        ("sqlserver", DisableTriggers) => format!("DISABLE TRIGGER ALL ON {}", table()?),
        ("sqlite", IntegrityCheck) => {
            format!("PRAGMA {}.integrity_check", schema.ok_or_else(unsupported)?)
        }
        ("sqlite", Vacuum) => format!("VACUUM {}", schema.ok_or_else(unsupported)?),
        // DuckDB vacuums and checkpoints the whole database.
        ("duckdb", Vacuum) => "VACUUM".to_string(),
        ("duckdb", Checkpoint) => "CHECKPOINT".to_string(),
        _ => return Err(unsupported()),
    })
}

/// Whether the engine refuses the action inside a transaction.
fn outside_transaction(kind: MaintenanceKind) -> bool {
    match kind {
        MaintenanceKind::Vacuum | MaintenanceKind::Checkpoint => true,
        MaintenanceKind::Analyze
        | MaintenanceKind::RefreshMaterializedView
        | MaintenanceKind::Truncate
        | MaintenanceKind::EnableTriggers
        | MaintenanceKind::DisableTriggers
        | MaintenanceKind::Check
        | MaintenanceKind::Repair
        | MaintenanceKind::Optimize
        | MaintenanceKind::RebuildTable
        | MaintenanceKind::RebuildIndexes
        | MaintenanceKind::IntegrityCheck => false,
    }
}

/// The plan the app shows: the statement, whether it runs outside a transaction, and what a
/// TRUNCATE … CASCADE also empties (the tables whose foreign keys point to it, and theirs, read
/// from the catalog now). Refused on a read-only connection or session, before anything is read.
/// `CONCURRENTLY` is checked against the catalog: PostgreSQL refreshes so only a populated view
/// with a unique index on plain columns.
pub async fn maintenance_plan(
    session: &Session,
    request: &MaintenanceRequest,
    read_only: bool,
) -> Result<MaintenancePlan, ObjectError> {
    if read_only {
        return Err(ObjectError::ReadOnly);
    }
    let dialect = session.dialect();
    let statement = maintenance_statement(dialect.as_ref(), request)?;
    let mut also_empties = Vec::new();
    if let ObjectTarget::Relation { schema, name, .. } = &request.target {
        if request.kind == MaintenanceKind::RefreshMaterializedView && request.options.concurrently
        {
            concurrently_possible(session, schema, name).await?;
        }
        if request.kind == MaintenanceKind::Truncate && request.options.cascade {
            also_empties = referencing_tables(session, schema, name).await?;
        }
    }
    Ok(MaintenancePlan {
        sql: format!("{statement};\n"),
        outside_transaction: outside_transaction(request.kind),
        also_empties,
    })
}

/// Whether PostgreSQL can refresh this materialized view concurrently.
async fn concurrently_possible(
    session: &Session,
    schema: &str,
    name: &str,
) -> Result<(), ObjectError> {
    let engine = session.dialect().name().to_string();
    let schema = sql_literal(&Value::Text(schema.to_string()), &engine);
    let name = sql_literal(&Value::Text(name.to_string()), &engine);
    let sql = format!(
        "SELECT CASE WHEN NOT m.ispopulated THEN 'empty' \
         WHEN NOT EXISTS (SELECT 1 FROM pg_index i JOIN pg_class c ON c.oid = i.indrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace WHERE n.nspname = m.schemaname \
         AND c.relname = m.matviewname AND i.indisunique AND i.indpred IS NULL \
         AND i.indexprs IS NULL) THEN 'no index' ELSE 'ok' END \
         FROM pg_matviews m WHERE m.schemaname = {schema} AND m.matviewname = {name}"
    );
    let answer = scalar_metadata(session, &sql)
        .await
        .map_err(|message| ObjectError::Failed {
            index: 0,
            total: 1,
            message,
        })?;
    match answer {
        Some(Value::Text(state)) if state == "ok" => Ok(()),
        Some(Value::Text(state)) if state == "empty" => Err(ObjectError::Unsupported(
            "the view has no data yet: refresh it once without CONCURRENTLY".into(),
        )),
        Some(Value::Text(_)) => Err(ObjectError::Unsupported(
            "CONCURRENTLY needs a unique index on plain columns of the view".into(),
        )),
        Some(_) | None => Err(ObjectError::Gone(name)),
    }
}

/// The tables a `TRUNCATE … CASCADE` of `schema.name` also empties: those whose foreign keys
/// point to it, then those pointing to them, each once, qualified for the engine.
async fn referencing_tables(
    session: &Session,
    schema: &str,
    name: &str,
) -> Result<Vec<String>, ObjectError> {
    let dialect = session.dialect();
    let mut seen: Vec<(String, String)> = vec![(schema.to_string(), name.to_string())];
    let mut queue = vec![(schema.to_string(), name.to_string())];
    let mut out = Vec::new();
    while let Some((schema, table)) = queue.pop() {
        let keys = session
            .connection()
            .list_referencing_keys(&schema, &table)
            .await
            .map_err(|e| ObjectError::Failed {
                index: 0,
                total: 1,
                message: e.to_string(),
            })?;
        for key in keys {
            let next = (key.schema.clone(), key.table.clone());
            if seen.contains(&next) {
                continue;
            }
            seen.push(next.clone());
            out.push(qualified_name(dialect.as_ref(), &key.schema, &[key.table]));
            queue.insert(0, next);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(schema: &str, name: &str) -> ObjectTarget {
        ObjectTarget::Relation {
            schema: schema.into(),
            name: name.into(),
            kind: ObjectKind::Table,
        }
    }

    fn sql(
        engine: &str,
        target: ObjectTarget,
        kind: MaintenanceKind,
        options: MaintenanceOptions,
    ) -> Result<String, ObjectError> {
        let dialect = spelaion_dialect::for_engine(engine).unwrap();
        maintenance_statement(
            dialect.as_ref(),
            &MaintenanceRequest {
                target,
                kind,
                options,
            },
        )
    }

    #[test]
    fn postgres_writes_vacuum_with_the_options_chosen_and_quotes_names() {
        use MaintenanceKind::*;
        assert_eq!(
            sql(
                "postgres",
                table("public", "orders"),
                Vacuum,
                MaintenanceOptions::default()
            )
            .unwrap(),
            "VACUUM public.orders"
        );
        let all = MaintenanceOptions {
            freeze: true,
            analyze: true,
            verbose: true,
            skip_locked: true,
            disable_page_skipping: true,
            index_cleanup: Some(IndexCleanup::Off),
            truncate_pages: Some(false),
            ..Default::default()
        };
        assert_eq!(
            sql("postgres", table("My Schema", "Order\"s"), Vacuum, all).unwrap(),
            "VACUUM (FREEZE, VERBOSE, ANALYZE, DISABLE_PAGE_SKIPPING, SKIP_LOCKED, INDEX_CLEANUP OFF, TRUNCATE false) \"My Schema\".\"Order\"\"s\""
        );
        assert!(
            sql(
                "postgres",
                table("public", "t"),
                Vacuum,
                MaintenanceOptions {
                    full: true,
                    disable_page_skipping: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
        assert_eq!(
            sql(
                "postgres",
                table("public", "t"),
                Truncate,
                MaintenanceOptions {
                    only: true,
                    restart_identity: true,
                    cascade: true,
                    ..Default::default()
                }
            )
            .unwrap(),
            "TRUNCATE ONLY public.t RESTART IDENTITY CASCADE"
        );
        assert_eq!(
            sql(
                "postgres",
                table("public", "t"),
                DisableTriggers,
                MaintenanceOptions::default()
            )
            .unwrap(),
            "ALTER TABLE public.t DISABLE TRIGGER USER"
        );
        let view = ObjectTarget::Relation {
            schema: "public".into(),
            name: "v".into(),
            kind: ObjectKind::MaterializedView,
        };
        assert_eq!(
            sql(
                "postgres",
                view.clone(),
                RefreshMaterializedView,
                MaintenanceOptions {
                    concurrently: true,
                    ..Default::default()
                }
            )
            .unwrap(),
            "REFRESH MATERIALIZED VIEW CONCURRENTLY public.v"
        );
        assert!(
            sql(
                "postgres",
                view.clone(),
                RefreshMaterializedView,
                MaintenanceOptions {
                    concurrently: true,
                    no_data: true,
                    ..Default::default()
                }
            )
            .is_err()
        );
        // A view is not truncated, and a table is not refreshed.
        assert!(sql("postgres", view, Truncate, MaintenanceOptions::default()).is_err());
        assert!(
            sql(
                "postgres",
                table("public", "t"),
                RefreshMaterializedView,
                MaintenanceOptions::default()
            )
            .is_err()
        );
    }

    #[test]
    fn each_engine_offers_only_its_own_actions() {
        use MaintenanceKind::*;
        assert_eq!(
            sql(
                "mysql",
                table("shop", "orders"),
                Check,
                MaintenanceOptions {
                    check_mode: Some(CheckMode::Extended),
                    ..Default::default()
                }
            )
            .unwrap(),
            "CHECK TABLE `shop`.`orders` EXTENDED"
        );
        assert_eq!(
            sql(
                "mysql",
                table("shop", "orders"),
                Optimize,
                MaintenanceOptions {
                    local: true,
                    ..Default::default()
                }
            )
            .unwrap(),
            "OPTIMIZE NO_WRITE_TO_BINLOG TABLE `shop`.`orders`"
        );
        assert_eq!(
            sql(
                "sqlserver",
                table("dbo", "orders"),
                RebuildIndexes,
                MaintenanceOptions::default()
            )
            .unwrap(),
            "ALTER INDEX ALL ON dbo.orders REBUILD"
        );
        let main = ObjectTarget::Schema {
            name: "main".into(),
        };
        assert_eq!(
            sql(
                "sqlite",
                main.clone(),
                IntegrityCheck,
                MaintenanceOptions::default()
            )
            .unwrap(),
            "PRAGMA main.integrity_check"
        );
        assert_eq!(
            sql(
                "duckdb",
                main.clone(),
                Checkpoint,
                MaintenanceOptions::default()
            )
            .unwrap(),
            "CHECKPOINT"
        );
        // Not offered: an error, never another engine's SQL.
        assert!(
            sql(
                "mysql",
                table("shop", "orders"),
                Vacuum,
                MaintenanceOptions::default()
            )
            .is_err()
        );
        assert!(
            sql(
                "sqlserver",
                table("dbo", "t"),
                Truncate,
                MaintenanceOptions::default()
            )
            .is_err()
        );
        assert!(
            sql(
                "duckdb",
                main.clone(),
                IntegrityCheck,
                MaintenanceOptions::default()
            )
            .is_err()
        );
        assert!(
            sql(
                "sqlite",
                table("main", "t"),
                Vacuum,
                MaintenanceOptions::default()
            )
            .is_err()
        );
        assert!(
            sql(
                "clickhouse",
                table("db", "t"),
                Optimize,
                MaintenanceOptions::default()
            )
            .is_err()
        );
    }
}
