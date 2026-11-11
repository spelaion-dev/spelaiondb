//! Confirmation before risky statements run from the editor: a `DELETE` or `UPDATE`
//! without `WHERE`, `DROP` and `TRUNCATE` everywhere, anything that is not a read where the
//! connection's type confirms data changes (Production), and every statement where it
//! confirms execution (ADR-029). It is a safety net against slips, not a permission barrier
//! (that is read-only mode, ADR-008): whatever the parser cannot read only asks when it starts
//! like a write that could hurt.
//!
//! The barrier of a read-only connection is here too (ADR-053): [`check_run`] refuses, before
//! anything runs and whatever was confirmed, a script with a statement that is not a read, a
//! session setting or transaction control. The session refuses it again when it runs.

use std::ops::ControlFlow;

use spelaion_config::Safeguards;
use spelaion_driver_api::{Dialect, StatementSpan};
use sqlparser::ast::{Statement, visit_statements};
use sqlparser::dialect::{
    Dialect as SqlDialect, DuckDbDialect, GenericDialect, MsSqlDialect, MySqlDialect,
    PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;

use crate::Session;
use crate::classify::{classify_read_only, classify_read_only_connection};

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
    let readings = readings(engine, sql);
    readings
        .iter()
        .find_map(|reading| destructive(engine, reading))
        .or_else(|| {
            (safeguards.confirm_data_changes
                && readings
                    .iter()
                    .any(|reading| classify_read_only(engine, reading).is_err()))
            .then_some(Risk::DataChange)
        })
        .or_else(|| safeguards.confirm_execution.then_some(Risk::Execution))
}

/// The ways the server may read `sql`, each checked: a statement is risky if any of them is.
/// MySQL and MariaDB run the body of an executable comment (`/*! … */`, `/*!50001 … */`, and
/// MariaDB's `/*M! … */`) only when the server's version and kind match it, which the
/// statement alone does not tell. sqlparser always runs `/*!` bodies and never `/*M!` ones;
/// the statement is also read with every such body run and with none run, so the guard asks
/// about what any server may run (and never less than sqlparser's reading alone). Other
/// engines' statements are read as they are.
fn readings(engine: &str, sql: &str) -> Vec<String> {
    if !matches!(engine, "mysql" | "mariadb") || executable_comment(sql).is_none() {
        return vec![sql.to_string()];
    }
    vec![
        sql.to_string(),
        executable_bodies_as_code(sql),
        // `/*!` becomes a plain comment; `/*M!` already is one for sqlparser.
        sql.replace("/*!", "/* "),
    ]
}

/// Where the next executable comment's marker starts in `sql`, and where its body starts
/// (after `/*!` or `/*M!` and the version digits).
fn executable_comment(sql: &str) -> Option<(usize, usize)> {
    let bytes = sql.as_bytes();
    (0..bytes.len()).find_map(|i| {
        let rest = &bytes[i..];
        let marker = if rest.starts_with(b"/*!") {
            3
        } else if rest.len() >= 4
            && rest.starts_with(b"/*")
            && rest[2].eq_ignore_ascii_case(&b'm')
            && rest[3] == b'!'
        {
            4
        } else {
            return None;
        };
        let digits = rest[marker..]
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        Some((i, i + marker + digits))
    })
}

/// `sql` with the body of every executable comment as plain code: `/*!50001 X */` is ` X `.
fn executable_bodies_as_code(sql: &str) -> String {
    let mut code = String::with_capacity(sql.len());
    let mut rest = sql;
    while let Some((marker, body)) = executable_comment(rest) {
        code.push_str(&rest[..marker]);
        code.push(' ');
        let after = &rest[body..];
        match after.find("*/") {
            Some(end) => {
                code.push_str(&after[..end]);
                code.push(' ');
                rest = &after[end + 2..];
            }
            None => {
                code.push_str(after);
                rest = "";
            }
        }
    }
    code.push_str(rest);
    code
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

/// A script a read-only connection refuses (ADR-053): nothing of it runs, whatever was
/// confirmed. It names the first statement refused and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the connection is read-only, so nothing was run: statement {}, \"{statement}\", is refused ({reason}). Only reads, session settings and transaction control run on a read-only connection",
    .index + 1
)]
pub struct ReadOnlyRefusal {
    /// Position among the script's statements.
    pub index: usize,
    pub span: StatementSpan,
    /// The start of the statement, to recognize it.
    pub statement: String,
    pub reason: String,
}

/// Whether every statement of `script`, split like a run, may run on a read-only connection
/// ([`classify_read_only_connection`]). The first one that may not is the refusal.
pub fn check_read_only(dialect: &dyn Dialect, script: &str) -> Result<(), ReadOnlyRefusal> {
    for (index, span) in dialect.split_statements(script).into_iter().enumerate() {
        let text = span.text(script);
        if let Err(reason) = classify_read_only_connection(dialect.name(), text) {
            return Err(ReadOnlyRefusal {
                index,
                span,
                statement: start_of(text),
                reason,
            });
        }
    }
    Ok(())
}

/// Why a script from the editor may not run on its session yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunRefused {
    /// The connection is read-only and the script is not: no confirmation changes that.
    #[error(transparent)]
    ReadOnly(ReadOnlyRefusal),
    #[error(transparent)]
    NeedsConfirmation(NeedsConfirmation),
}

/// Whether `script` may run on `session` from the editor: never when the session is read-only
/// and refuses it (ADR-053), whatever was confirmed; then [`check_script`] under the
/// connection type's safeguards.
pub fn check_run(
    session: &Session,
    script: &str,
    safeguards: &Safeguards,
    confirmed: bool,
) -> Result<(), RunRefused> {
    let dialect = session.dialect();
    if session.is_read_only() {
        check_read_only(dialect.as_ref(), script).map_err(RunRefused::ReadOnly)?;
    }
    check_script(dialect.as_ref(), script, safeguards, confirmed)
        .map_err(RunRefused::NeedsConfirmation)
}

/// What the app asks to confirm before running `script` on `session`: nothing when a read-only
/// session refuses it anyway (the run fails at once, so there is nothing to confirm),
/// otherwise [`script_risks`].
pub fn run_risks(session: &Session, script: &str, safeguards: &Safeguards) -> Vec<RiskyStatement> {
    let dialect = session.dialect();
    if session.is_read_only() && check_read_only(dialect.as_ref(), script).is_err() {
        return Vec::new();
    }
    script_risks(dialect.as_ref(), script, safeguards)
}

/// The first line of a statement, at most 80 characters.
fn start_of(statement: &str) -> String {
    let trimmed = statement.trim();
    let line = trimmed.lines().next().unwrap_or_default().trim_end();
    let mut out: String = line.chars().take(80).collect();
    if out.len() < trimmed.len() {
        out.push('…');
    }
    out
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

    /// The MySQL and MariaDB statement's risk, from every reading.
    fn mysql_risk(sql: &str, safeguards: &Safeguards) -> Option<Risk> {
        statement_risk("mysql", sql, safeguards)
    }

    #[test]
    fn executable_comments_are_read_both_run_and_skipped() {
        // What a server runs inside them is confirmed like any statement, MariaDB's too.
        for (sql, risk) in [
            ("/*!50000 drop table t */", Risk::Drop),
            ("/*!50000 DROP EVENT e */", Risk::Drop),
            ("/*!delete from t */", Risk::DeleteAll),
            ("/*M!100000 drop table t */", Risk::Drop),
            ("/*m!100000 truncate table t */", Risk::Truncate),
            (
                "-- c
/*M!100000 update t set a = 1 */",
                Risk::UpdateAll,
            ),
            ("select 1 /*M!100000 ; delete from t */", Risk::DeleteAll),
            // A server older than the comment, or MySQL with MariaDB's, skips the body: the
            // WHERE it would have added is not there.
            ("delete from t /*!99999 where id = 1 */", Risk::DeleteAll),
            (
                "update t set a = 1 /*M!100000 where id = 1 */",
                Risk::UpdateAll,
            ),
        ] {
            assert_eq!(mysql_risk(sql, &NONE), Some(risk), "{sql}");
        }
        for sql in [
            "/*!40101 SET @x = 1 */",
            "/*M!100616 SET @x = 1 */",
            "/*!50001 CREATE ALGORITHM=UNDEFINED */ /*!50001 VIEW v AS select 1 AS a */",
            "delete from t where id = 1 /*!50000 and a > 0 */",
            "select '/*!', 1",
        ] {
            assert_eq!(mysql_risk(sql, &NONE), None, "{sql}");
        }
        // Production confirms a data change in either reading.
        assert_eq!(
            mysql_risk("select 1 /*M!100000 , sleep(0) into @x */", &PRODUCTION),
            Some(Risk::DataChange)
        );
        assert_eq!(mysql_risk("select 1 /*!50000 , 2 */", &PRODUCTION), None);
        // Other engines keep them as comments.
        assert_eq!(
            statement_risk("postgres", "/*!50000 drop table t */", &NONE),
            None
        );
    }

    #[test]
    fn executable_comment_bodies_become_code() {
        assert_eq!(
            executable_bodies_as_code("a /*!50001 b */ c /*M!100616 d*/ /* e */ /*!f"),
            "a   b   c   d  /* e */  f"
        );
        assert_eq!(executable_bodies_as_code("no comments"), "no comments");
        assert_eq!(executable_comment("/* M!1 */"), None);
    }
}
