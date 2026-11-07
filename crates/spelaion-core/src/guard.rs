//! Confirmation before risky statements run from the editor: a `DELETE` or `UPDATE`
//! without `WHERE`, `DROP` and `TRUNCATE` everywhere, anything that is not a read where the
//! connection's type confirms data changes (Production), and every statement where it
//! confirms execution (ADR-029). It is a safety net against slips, not a permission barrier
//! (that is read-only mode, ADR-008): whatever the parser cannot read only asks when it starts
//! like a write that could hurt.

use std::ops::ControlFlow;

use spelaion_config::Safeguards;
use spelaion_driver_api::{Dialect, StatementSpan};
use sqlparser::ast::{Statement, visit_statements};
use sqlparser::dialect::{
    Dialect as SqlDialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect,
    PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;

use crate::classify::classify_read_only;

/// Why a statement asks for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// `DELETE` without `WHERE`: every row goes.
    DeleteAll,
    /// `UPDATE` without `WHERE`: every row changes.
    UpdateAll,
    Drop,
    Truncate,
    /// A `DELETE` or `UPDATE` the parser could not read, so its `WHERE` was not checked.
    UncheckedWrite,
    /// Not a read, where the connection's type confirms data changes (Production).
    DataChange,
    /// Any statement, where the connection's type confirms every execution.
    Execution,
}

/// A statement of the script that needs confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RiskyStatement {
    /// Position among the script's statements.
    pub index: usize,
    pub span: StatementSpan,
    pub risk: Risk,
}

/// The statements of `script` to confirm before running it, split like a run, under the
/// safeguards of the connection's type. Each statement has its most serious risk.
pub fn script_risks(
    dialect: &dyn Dialect,
    script: &str,
    safeguards: &Safeguards,
) -> Vec<RiskyStatement> {
    dialect
        .split_statements(script)
        .into_iter()
        .enumerate()
        .filter_map(|(index, span)| {
            statement_risk(dialect.name(), span.text(script), safeguards)
                .map(|risk| RiskyStatement { index, span, risk })
        })
        .collect()
}

fn statement_risk(engine: &str, sql: &str, safeguards: &Safeguards) -> Option<Risk> {
    destructive(engine, sql)
        .or_else(|| {
            (safeguards.confirm_data_changes && classify_read_only(engine, sql).is_err())
                .then_some(Risk::DataChange)
        })
        .or_else(|| safeguards.confirm_execution.then_some(Risk::Execution))
}

/// A script run from the editor that skipped the confirmation it needed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("this script has statements that need confirmation before running")]
pub struct NeedsConfirmation {
    pub risks: Vec<RiskyStatement>,
}

/// Whether `script` may run: always once the user confirmed what [`script_risks`] listed,
/// otherwise only when it lists nothing. The app asks first; a run that skipped the question
/// is refused here, whatever the UI did.
pub fn check_script(
    dialect: &dyn Dialect,
    script: &str,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<(), NeedsConfirmation> {
    if confirmed {
        return Ok(());
    }
    let risks = script_risks(dialect, script, safeguards);
    if risks.is_empty() {
        Ok(())
    } else {
        Err(NeedsConfirmation { risks })
    }
}

pub(crate) fn destructive(engine: &str, sql: &str) -> Option<Risk> {
    let first = first_word(sql);
    // Every kind of DROP (table, schema, database, function…), parsed or not.
    if first == "drop" {
        return Some(Risk::Drop);
    }
    let parser = sql_dialect(engine);
    let Ok(statements) = Parser::parse_sql(parser.as_ref(), sql) else {
        return match first.as_str() {
            "truncate" => Some(Risk::Truncate),
            "delete" | "update" => Some(Risk::UncheckedWrite),
            _ => None,
        };
    };
    let mut found = None;
    // Also inside other statements: `WITH gone AS (DELETE FROM t RETURNING *) SELECT …`.
    let _ = visit_statements(&statements, |statement| {
        found = match statement {
            Statement::Delete(delete) if delete.selection.is_none() => Some(Risk::DeleteAll),
            Statement::Update(update) if update.selection.is_none() => Some(Risk::UpdateAll),
            Statement::Truncate(_) => Some(Risk::Truncate),
            Statement::Drop { .. } => Some(Risk::Drop),
            _ => None,
        };
        if found.is_some() {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
    found
}

/// sqlparser's dialect for one of our engines (by `Dialect::name`).
pub(crate) fn sql_dialect(engine: &str) -> Box<dyn SqlDialect> {
    match engine {
        "postgres" => Box::new(PostgreSqlDialect {}),
        "sqlite" => Box::new(SQLiteDialect {}),
        "mysql" | "mariadb" => Box::new(MySqlDialect {}),
        "duckdb" => Box::new(DuckDbDialect {}),
        "sqlserver" => Box::new(MsSqlDialect {}),
        _ => Box::new(GenericDialect {}),
    }
}

/// The first word of the statement in lower case, after comments and spaces.
fn first_word(sql: &str) -> String {
    let mut rest = sql;
    loop {
        rest = rest.trim_start();
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, next)| next);
        } else if let Some(after) = rest.strip_prefix('#') {
            // MySQL comment.
            rest = after.split_once('\n').map_or("", |(_, next)| next);
        } else {
            break;
        }
    }
    rest.chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use spelaion_driver_sqlite::SqliteDialect;

    const NONE: Safeguards = Safeguards {
        confirm_execution: false,
        confirm_data_changes: false,
        close_idle_after: None,
        manual_commit: false,
        smart_commit: false,
    };
    const PRODUCTION: Safeguards = Safeguards {
        confirm_execution: false,
        confirm_data_changes: true,
        close_idle_after: None,
        manual_commit: false,
        smart_commit: false,
    };
    const EVERY_RUN: Safeguards = Safeguards {
        confirm_execution: true,
        confirm_data_changes: false,
        close_idle_after: None,
        manual_commit: false,
        smart_commit: false,
    };

    fn risks(script: &str, production: bool) -> Vec<(usize, Risk)> {
        risks_with(script, if production { &PRODUCTION } else { &NONE })
    }

    fn risks_with(script: &str, safeguards: &Safeguards) -> Vec<(usize, Risk)> {
        script_risks(&SqliteDialect, script, safeguards)
            .into_iter()
            .map(|r| (r.index, r.risk))
            .collect()
    }

    #[test]
    fn destructive_statements_ask_everywhere() {
        let script = "select 1;
            delete from t;
            delete from t where id = 1;
            update t set x = 1;
            UPDATE t SET x = 1 WHERE id = 2;
            /* tidy */ -- up
            DROP TABLE t;
            drop view if exists v;
            insert into t values (1)";
        assert_eq!(
            risks(script, false),
            [
                (1, Risk::DeleteAll),
                (3, Risk::UpdateAll),
                (5, Risk::Drop),
                (6, Risk::Drop)
            ]
        );
    }

    #[test]
    fn writes_hidden_in_ctes_and_other_dialects_are_found() {
        assert_eq!(
            destructive(
                "postgres",
                "with gone as (delete from t returning *) select * from gone"
            ),
            Some(Risk::DeleteAll)
        );
        assert_eq!(
            destructive("postgres", "truncate table t"),
            Some(Risk::Truncate)
        );
        assert_eq!(destructive("mysql", "truncate `t`"), Some(Risk::Truncate));
        assert_eq!(
            destructive("mysql", "delete from `t` limit 10"),
            Some(Risk::DeleteAll)
        );
        assert_eq!(
            destructive("mysql", "# note\nDROP DATABASE shop"),
            Some(Risk::Drop)
        );
        assert_eq!(
            destructive("mysql", "update `t` set a = 1 where b = 2"),
            None
        );
    }

    #[test]
    fn unreadable_writes_still_ask() {
        assert_eq!(
            destructive("sqlite", "delete frm t where x"),
            Some(Risk::UncheckedWrite)
        );
        assert_eq!(
            destructive("sqlite", "truncate ??? t"),
            Some(Risk::Truncate)
        );
        assert_eq!(destructive("sqlite", "selec 1"), None);
    }

    #[test]
    fn production_asks_for_every_write_but_not_for_reads() {
        let script = "select * from t; insert into t values (1); explain select 1; delete from t";
        assert_eq!(
            risks(script, true),
            [(1, Risk::DataChange), (3, Risk::DeleteAll)]
        );
        assert!(risks("insert into t values (1)", false).is_empty());
    }

    #[test]
    fn confirming_execution_asks_for_every_statement_even_reads() {
        let script = "select 1; insert into t values (1); drop table t";
        assert_eq!(
            risks_with(script, &EVERY_RUN),
            [(0, Risk::Execution), (1, Risk::Execution), (2, Risk::Drop)]
        );
        let both = PRODUCTION.tighten(EVERY_RUN);
        assert_eq!(
            risks_with(script, &both),
            [(0, Risk::Execution), (1, Risk::DataChange), (2, Risk::Drop)]
        );
        // Comments alone and an empty script have nothing to run.
        assert!(risks_with("  -- nothing\n", &EVERY_RUN).is_empty());
    }

    #[test]
    fn a_run_that_skipped_the_question_is_refused() {
        let check = |script: &str, safeguards: &Safeguards, confirmed: bool| {
            check_script(&SqliteDialect, script, safeguards, confirmed)
        };
        assert!(check("select 1", &NONE, false).is_ok());
        // Every way around the question: reads, comments, case, several statements and a
        // statement the parser cannot read.
        for script in [
            "select 1",
            "/* hi */ SELECT 1",
            "select 1; select 2",
            "explain select 1",
            "selec 1",
        ] {
            assert!(check(script, &EVERY_RUN, false).is_err(), "{script}");
            assert!(check(script, &EVERY_RUN, true).is_ok(), "{script}");
        }
        // A write after a read, in a CTE, DDL, a pragma, and a trailing comment.
        for script in [
            "select 1; insert into t values (1)",
            "INSERT INTO t VALUES (1)",
            "with x as (select 1) insert into t select * from x",
            "create table t2 (a int)",
            "pragma foreign_keys = off",
            "insert into t values (1) -- select",
        ] {
            assert!(check(script, &PRODUCTION, false).is_err(), "{script}");
            assert!(check(script, &PRODUCTION, true).is_ok(), "{script}");
        }
        assert!(check("select * from t", &PRODUCTION, false).is_ok());
        assert!(check("delete from t", &NONE, false).is_err());
    }
}
