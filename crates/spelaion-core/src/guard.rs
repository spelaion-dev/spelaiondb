//! Confirmation before risky statements run from the editor: a `DELETE` or `UPDATE`
//! without `WHERE`, `DROP` and `TRUNCATE` everywhere, anything that is not a read where the
//! connection's type confirms data changes (Production), and every statement where it
//! confirms execution (ADR-029). It is a safety net against slips, not a permission barrier
//! (that is read-only mode, ADR-008): whatever the parser cannot read only asks when it starts
//! like a write that could hurt, or, in a SQL Server batch (statements need no `;` there), when
//! such a write's keyword is in it outside texts, names and comments.
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
use sqlparser::tokenizer::{Token, Tokenizer};

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
        let by_first_word = match first.as_str() {
            "truncate" => Some(Risk::Truncate),
            "delete" | "update" => Some(Risk::UncheckedWrite),
            _ => None,
        };
        return match engine {
            "sqlserver" => by_first_word.or_else(|| tsql_unparsed_risk(sql)),
            _ => by_first_word,
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

/// The risk of a T-SQL batch sqlparser cannot read. T-SQL needs no `;` between statements and
/// one batch often holds several (the splitter keeps them together, and a `DECLARE` keeps the
/// rest of its batch), so a risky statement may follow one the parser cannot read. The batch
/// is read with sqlparser's tokenizer, which knows strings, `[…]` names and comments without
/// needing the batch to parse: split at its `;` into statements, each checked again; and in a
/// statement that still does not parse, a `DROP`, `TRUNCATE`, `DELETE` or `UPDATE` word
/// anywhere asks (its `WHERE` could not be checked). It may ask once too often (`ALTER TABLE …
/// DROP COLUMN`), the right side for a safety net. The body of a procedure, function, trigger
/// or view does not run when it is created: its definition asks nothing, as before. `None`
/// when the tokenizer cannot read the batch either (an open quote).
fn tsql_unparsed_risk(sql: &str) -> Option<Risk> {
    let tokens = Tokenizer::new(&MsSqlDialect {}, sql).tokenize().ok()?;
    if defines_module(&tokens) {
        return None;
    }
    let statements = tsql_statements(&tokens);
    if statements.len() > 1 {
        return statements
            .iter()
            .find_map(|statement| destructive("sqlserver", statement));
    }
    risky_words(&tokens)
}

/// A word token outside quotes (`[drop]` and `"drop"` are names), in lower case.
fn plain_word(token: &Token) -> Option<String> {
    match token {
        Token::Word(word) if word.quote_style.is_none() => Some(word.value.to_ascii_lowercase()),
        _ => None,
    }
}

/// `CREATE [OR ALTER] | ALTER` of a procedure, function, trigger or view: the rest of the
/// batch is its body.
fn defines_module(tokens: &[Token]) -> bool {
    let words: Vec<String> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .take(4)
        .map(|token| plain_word(token).unwrap_or_default())
        .collect();
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    let kind = match words.as_slice() {
        ["create", "or", "alter", kind, ..] | ["create" | "alter", kind, ..] => *kind,
        _ => return false,
    };
    matches!(kind, "procedure" | "proc" | "function" | "trigger" | "view")
}

/// The most serious risky word of a statement the parser cannot read. A few uses change no
/// rows and do not count, each only where nothing else can be meant: a foreign key's `ON
/// DELETE` / `ON UPDATE` action (after `REFERENCES`, followed by `CASCADE`, `RESTRICT`, `SET
/// NULL`, `SET DEFAULT` or `NO ACTION`: `SET NOCOUNT ON` followed by a `DELETE` still asks), a cursor's `FOR UPDATE`
/// (after `CURSOR`), `UPDATE STATISTICS` (`STATISTICS` is reserved: a table of that name is
/// written `[statistics]`), privilege names in `GRANT`, `REVOKE` and `DENY` (up to their `ON`,
/// `TO` or `FROM`) and a role's `DROP MEMBER`. A statement that hides a write behind one of
/// these does not compile, and a batch that does not compile runs nothing.
fn risky_words(tokens: &[Token]) -> Option<Risk> {
    let words: Vec<String> = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .map(|token| plain_word(token).unwrap_or_default())
        .collect();
    let mut found = None;
    // Inside `GRANT` / `REVOKE` / `DENY`, up to its `ON`, `TO` or `FROM`: privilege names.
    let mut privileges = false;
    for (i, word) in words.iter().enumerate() {
        match word.as_str() {
            "grant" | "revoke" | "deny" => privileges = true,
            "on" | "to" | "from" => privileges = false,
            _ => {}
        }
        if privileges {
            continue;
        }
        let before = i.checked_sub(1).map(|j| words[j].as_str());
        let after = words.get(i + 1).map(String::as_str);
        let earlier = |w: &str| words[..i].iter().any(|seen| seen == w);
        let then = words.get(i + 2).map(String::as_str);
        let referential_action = before == Some("on")
            && earlier("references")
            && match after {
                Some("cascade" | "restrict") => true,
                Some("set") => matches!(then, Some("null" | "default")),
                Some("no") => then == Some("action"),
                _ => false,
            };
        let cursor_option = word == "update" && before == Some("for") && earlier("cursor");
        let statistics = word == "update" && after == Some("statistics");
        let risk = match word.as_str() {
            // `ALTER [SERVER] ROLE … DROP MEMBER`: a role membership, not an object.
            "drop" if after == Some("member") && earlier("role") => continue,
            "drop" => Risk::Drop,
            "truncate" => Risk::Truncate,
            "delete" | "update" if !(referential_action || cursor_option || statistics) => {
                Risk::UncheckedWrite
            }
            _ => continue,
        };
        let serious = |risk: Risk| match risk {
            Risk::Drop => 3,
            Risk::Truncate => 2,
            _ => 1,
        };
        if found.is_none_or(|current| serious(risk) > serious(current)) {
            found = Some(risk);
        }
    }
    found
}

/// The statements of a T-SQL batch, split where the SQL Server splitter splits outside a
/// `DECLARE`: at `;` outside quotes, comments and `BEGIN … END` / `CASE … END` blocks (`BEGIN
/// TRAN` and the like are not blocks).
fn tsql_statements(tokens: &[Token]) -> Vec<String> {
    let next_word = |from: usize| tokens[from + 1..].iter().find_map(plain_word);
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    for (i, token) in tokens.iter().enumerate() {
        match (token, plain_word(token).as_deref()) {
            (Token::SemiColon, _) if depth == 0 => {
                statements.push(std::mem::take(&mut current));
                continue;
            }
            (_, Some("begin")) => {
                if !matches!(
                    next_word(i).as_deref(),
                    Some("tran" | "transaction" | "distributed" | "dialog" | "conversation")
                ) {
                    depth += 1;
                }
            }
            (_, Some("case")) => depth += 1,
            (_, Some("end")) if next_word(i).as_deref() != Some("conversation") => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        current.push_str(&token.to_string());
    }
    statements.push(current);
    statements.retain(|statement| !statement.trim().is_empty());
    statements
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
    use spelaion_driver_sqlserver::SqlServerDialect;

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

    #[test]
    fn tsql_batches_split_like_the_sqlserver_splitter() {
        let tokens = Tokenizer::new(
            &MsSqlDialect {},
            "declare @n int = 0; while @n < 3 begin set @n += 1; end;              select case when 1 = 1 then 'a;b' end; begin tran; /* c; */ delete from t",
        )
        .tokenize()
        .unwrap();
        let parts = tsql_statements(&tokens);
        let parts: Vec<&str> = parts.iter().map(|p| p.trim()).collect();
        assert_eq!(
            parts,
            [
                "declare @n int = 0",
                "while @n < 3 begin set @n += 1; end",
                "select case when 1 = 1 then 'a;b' end",
                "begin tran",
                "/* c; */ delete from t"
            ]
        );
        assert_eq!(
            destructive(
                "sqlserver",
                "declare @n int = 0; raiserror('x', 0, 1); delete from t"
            ),
            Some(Risk::DeleteAll)
        );
    }

    fn tsql_risk(sql: &str) -> Option<Risk> {
        destructive("sqlserver", sql)
    }

    #[test]
    fn tsql_batches_without_semicolons_still_ask() {
        // The card's three batches: each runs a risky statement after another one.
        for (sql, risk) in [
            ("SELECT * FROM orders\nDROP TABLE orders", Risk::Drop),
            ("SET NOCOUNT ON\nDELETE FROM orders", Risk::UncheckedWrite),
            ("USE shop\nTRUNCATE TABLE orders", Risk::Truncate),
        ] {
            assert_eq!(tsql_risk(sql), Some(risk), "{sql}");
            let refused =
                check_script(&SqlServerDialect, sql, &NONE, false).expect_err("not confirmed");
            assert_eq!(refused.risks[0].risk, risk, "{sql}");
            assert!(check_script(&SqlServerDialect, sql, &NONE, true).is_ok());
        }
        // In a DECLARE's batch (SDB-112), with and without semicolons.
        assert_eq!(
            tsql_risk("declare @x int = 1; delete from t"),
            Some(Risk::DeleteAll)
        );
        assert_eq!(
            tsql_risk("DECLARE @x int = 0\nWHILE @x < 3 BEGIN SET @x += 1 END\nDELETE FROM t"),
            Some(Risk::UncheckedWrite)
        );
        assert_eq!(
            tsql_risk("declare @x int = 0;\nraiserror('x', 0, 1)\nupdate t set a = 1"),
            Some(Risk::UncheckedWrite)
        );
        // The most serious word decides; any case, any spacing.
        assert_eq!(
            tsql_risk("print 'x'\n\tUpDaTe t SET a = 1\nDrOp TABLE t"),
            Some(Risk::Drop)
        );
        assert_eq!(
            tsql_risk("select 1 /* note */ drop table t"),
            Some(Risk::Drop)
        );
        assert_eq!(tsql_risk("select 1 -- x\ndrop table t"), Some(Risk::Drop));
        // A backslash escapes nothing in T-SQL: the string ends at the second quote.
        assert_eq!(tsql_risk(r"select 'a\' drop table t --'"), Some(Risk::Drop));
        assert_eq!(tsql_risk("select 'it''s' drop table t"), Some(Risk::Drop));
        assert_eq!(
            tsql_risk("select N'x' truncate table t"),
            Some(Risk::Truncate)
        );
        // What looks like a word that changes no rows, used to hide a write.
        for sql in [
            "SET NOCOUNT ON\nUPDATE t SET a = 1",
            "set ansi_nulls on\ndelete t",
            "select a from t for\nupdate t set a = 1",
            "print 'x'\ncreate table c (p int references p)\nset xact_abort on\ndelete from t",
            "print 'references cursor'\nset nocount on update t set a = 1",
            "print 'x'\ncreate table c (p int references p)\nset nocount on\nupdate no set a = 1",
            "grant select on t to u\nupdate t set a = 1",
            "revoke select on t from u delete from t",
            "print 'role'\nselect 1\ndelete from t",
        ] {
            assert_eq!(tsql_risk(sql), Some(Risk::UncheckedWrite), "{sql}");
        }
        assert_eq!(
            tsql_risk("alter role r drop member u\ndrop table t"),
            Some(Risk::Drop)
        );
        // The fallback may ask once too often (a column dropped from a table).
        assert_eq!(
            tsql_risk("select 1\nalter table t drop column c"),
            Some(Risk::Drop)
        );
    }

    #[test]
    fn tsql_words_in_texts_names_and_comments_do_not_ask() {
        for sql in [
            "select 'drop table t'",
            "select [drop] from t",
            "select 1 -- drop table t",
            "print 'x'\nselect [delete], [update] from t",
            "print 'x'\nselect \"drop\" from t",
            "print 'x'\nselect 'a; drop table t; ''b'''",
            "print 'x'\nselect [a]]drop table t]",
            // T-SQL block comments nest: all of this is one comment.
            "print 'x'\n/* a /* b */ drop table t */ select 1",
            "print 'x'\nselect @delete, @update from t",
            // Words that change no rows.
            "print 'x'\ncreate table c (p int references p on delete cascade on update no action)",
            "print 'x'\nupdate statistics t",
            "ALTER SERVER ROLE dbcreator DROP MEMBER priv_login",
            "print 'x'\nalter role r drop member u",
            "print 'x'\ngrant select, update, delete on t to u",
            "print 'x'\ndeny update (a) on t to u\nrevoke delete on schema::s from u",
            "declare c cursor for select a from t for update\nopen c",
            // A procedure's body runs later, not when it is created.
            "create procedure p as\nselect 1\ndelete from t",
            "create or alter trigger tr on t after insert as\nprint 'x'\ndrop table u",
            "alter procedure p as select 1 truncate table t",
        ] {
            assert_eq!(tsql_risk(sql), None, "{sql}");
        }
        // An open quote: the tokenizer cannot read it, the first word decides as before.
        assert_eq!(tsql_risk("print 'x\ndrop table t"), None);
        assert_eq!(
            tsql_risk("delete from t where a = 'x"),
            Some(Risk::UncheckedWrite)
        );
        // Other engines keep their first-word fallback.
        assert_eq!(destructive("sqlite", "select 1 drop table t"), None);
    }
}
