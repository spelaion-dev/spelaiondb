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
use spelaion_driver_api::{Dialect, RequestClass, StatementSpan};
use sqlparser::ast::{BinaryOperator, Expr, Statement, Value as SqlValue, visit_statements};
use sqlparser::dialect::{
    BigQueryDialect, ClickHouseDialect, Dialect as SqlDialect, DuckDbDialect, GenericDialect,
    HiveDialect, MsSqlDialect, MySqlDialect, OracleDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::Session;
use crate::classify::{classify_read_only, classify_read_only_connection};
use crate::sql_mode::{ModeQuoting, as_default_mode};

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
    let mode = ModeQuoting::of(dialect);
    dialect
        .split_statements(script)
        .into_iter()
        .enumerate()
        .filter_map(|(index, span)| {
            let text = span.text(script);
            match request_risk(dialect, text, safeguards) {
                Some(risk) => risk,
                None => statement_risk(dialect.name(), text, mode, safeguards),
            }
            .map(|risk| RiskyStatement { index, span, risk })
        })
        .collect()
}

/// The risk of a statement of a language that is not SQL (ADR-056, point 4), read from the
/// request the core makes of it, so every way of writing a command is judged alike: `None`
/// for a language that is SQL. A drop, or a delete or update of every document, always asks;
/// a write, and whatever the core cannot tell (never taken for a read), ask where the
/// connection's type confirms data changes; every statement where it confirms execution. A
/// statement the core cannot read asks as one it cannot tell: running it fails anyway.
fn request_risk(
    dialect: &dyn Dialect,
    text: &str,
    safeguards: &Safeguards,
) -> Option<Option<Risk>> {
    // The database does not change what a command is. A language shaped like SQL that
    // `sqlparser` does not read (CQL) has lists of its own, read from its text.
    let class = match dialect.to_request(text, "guard") {
        Some(Ok(request)) => dialect.request_class(&request),
        Some(Err(_)) => RequestClass::Unknown,
        None => dialect.statement_class(text)?,
    };
    let risk = match class {
        RequestClass::Drop => Some(Risk::Drop),
        RequestClass::DeleteAll => Some(Risk::DeleteAll),
        RequestClass::UpdateAll => Some(Risk::UpdateAll),
        RequestClass::Write | RequestClass::Unknown if safeguards.confirm_data_changes => {
            Some(Risk::DataChange)
        }
        RequestClass::Read | RequestClass::Write | RequestClass::Unknown => None,
    };
    Some(risk.or_else(|| safeguards.confirm_execution.then_some(Risk::Execution)))
}

fn statement_risk(
    engine: &str,
    sql: &str,
    mode: Option<ModeQuoting>,
    safeguards: &Safeguards,
) -> Option<Risk> {
    let readings = readings(engine, sql, mode);
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
///
/// The session's own `sql_mode` may read quotes otherwise (SDB-262): under
/// `NO_BACKSLASH_ESCAPES` a string ends at a `'` the default mode reads as an escaped quote,
/// and under `ANSI_QUOTES` `"…"` is a name. Then the statement is also read as the session's
/// mode reads it, rewritten as the same statement in the default mode
/// ([`as_default_mode`]): the default reading is always one of the readings, so the guard
/// never asks less than it did, and the user's own mode is never changed.
fn readings(engine: &str, sql: &str, mode: Option<ModeQuoting>) -> Vec<String> {
    let mut bases = vec![sql.to_string()];
    if let Some(mode) = mode {
        let session = as_default_mode(sql, mode);
        if session != sql {
            bases.push(session);
        }
    }
    if !matches!(engine, "mysql" | "mariadb") {
        return bases;
    }
    let mut out: Vec<String> = Vec::new();
    for base in bases {
        let variants = if executable_comment(&base).is_none() {
            vec![base]
        } else {
            vec![
                base.clone(),
                executable_bodies_as_code(&base),
                // `/*!` becomes a plain comment; `/*M!` already is one for sqlparser.
                base.replace("/*!", "/* "),
            ]
        };
        for variant in variants {
            if !out.contains(&variant) {
                out.push(variant);
            }
        }
    }
    out
}

/// The texts a read-only connection checks for one statement: as written, and as the
/// session's own `sql_mode` reads it when that reads quotes otherwise (SDB-262). Every one must
/// pass.
fn read_only_readings(dialect: &dyn Dialect, sql: &str) -> Vec<String> {
    let mut out = vec![sql.to_string()];
    if let Some(mode) = ModeQuoting::of(dialect) {
        let session = as_default_mode(sql, mode);
        if session != sql {
            out.push(session);
        }
    }
    out
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

/// A script a read-only connection or session refuses (ADR-053): nothing of it runs, whatever was
/// confirmed. It names the first statement refused and why.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "{subject} is read-only, so nothing was run: statement {}, \"{statement}\", is refused ({reason}). Only reads, session settings and transaction control run on a read-only {scope}",
    .index + 1,
    subject = if *.session_only { "this session" } else { "the connection" },
    scope = if *.session_only { "session" } else { "connection" }
)]
pub struct ReadOnlyRefusal {
    /// Position among the script's statements.
    pub index: usize,
    pub span: StatementSpan,
    /// The start of the statement, to recognize it.
    pub statement: String,
    pub reason: String,
    session_only: bool,
}

impl ReadOnlyRefusal {
    pub(crate) fn for_session(mut self, session: &Session) -> Self {
        self.session_only = !session.is_connection_read_only() && session.is_session_read_only();
        self
    }
}

/// Whether every statement of `script`, split like a run, may run on a read-only connection
/// ([`classify_read_only_connection`]). The first one that may not is the refusal.
pub fn check_read_only(dialect: &dyn Dialect, script: &str) -> Result<(), ReadOnlyRefusal> {
    for (index, span) in dialect.split_statements(script).into_iter().enumerate() {
        let text = span.text(script);
        let refused = read_only_readings(dialect, text)
            .iter()
            .find_map(|reading| classify_read_only_connection(dialect.name(), reading).err());
        if let Some(reason) = refused {
            return Err(ReadOnlyRefusal {
                index,
                span,
                statement: start_of(text),
                reason,
                session_only: false,
            });
        }
    }
    Ok(())
}

/// Why a script from the editor may not run on its session yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunRefused {
    /// The session is read-only and the script is not: no confirmation changes that.
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
        check_read_only(dialect.as_ref(), script)
            .map_err(|refusal| RunRefused::ReadOnly(refusal.for_session(session)))?;
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
    if engine == "clickhouse"
        && first == "alter"
        && let Some(risk) = clickhouse_alter_risk(sql)
    {
        return Some(risk);
    }
    if matches!(engine, "hive" | "spark" | "kyuubi" | "impala")
        && let Some(risk) = hive_risk(sql)
    {
        return Some(risk);
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
            "oracle" => by_first_word.or_else(|| oracle_unparsed_risk(sql)),
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

/// The risk of an Oracle statement sqlparser cannot read, which is how a PL/SQL block reads
/// (ADR-059). Defining a procedure, function, package, trigger or type runs nothing: it asks
/// nothing. An anonymous block (`DECLARE`, `BEGIN`) runs what it holds: a `DROP`, `TRUNCATE`,
/// `DELETE` or `UPDATE` word in it asks (its `WHERE` could not be checked), and so does dynamic
/// SQL (`EXECUTE IMMEDIATE`, `DBMS_SQL`), whose statement is a string the guard cannot read. An
/// anonymous block the tokenizer cannot read either asks too, rather than nothing.
fn oracle_unparsed_risk(sql: &str) -> Option<Risk> {
    if !spelaion_dialect::oracle::is_plsql(sql, 0) {
        return None;
    }
    let first = first_word(sql);
    if !matches!(first.as_str(), "declare" | "begin") {
        // A unit's definition, or a query with `WITH FUNCTION`: nothing runs as it is made.
        return None;
    }
    let Ok(tokens) = Tokenizer::new(&OracleDialect {}, sql).tokenize() else {
        return Some(Risk::UncheckedWrite);
    };
    let words: Vec<String> = tokens.iter().filter_map(plain_word).collect();
    let dynamic = words
        .windows(2)
        .any(|w| w[0] == "execute" && w[1] == "immediate")
        || words.iter().any(|w| w == "dbms_sql");
    risky_words(&tokens).or(dynamic.then_some(Risk::UncheckedWrite))
}

/// The risk of a ClickHouse `ALTER TABLE`, which is also how it changes rows and wipes data,
/// each as the closest risk the guard has:
/// - drops: `… DROP COLUMN | PARTITION | INDEX …`, `… MOVE PARTITION … TO TABLE other` (the
///   partition leaves the table; `TO DISK | VOLUME` only moves its files) and `… REPLACE
///   PARTITION … FROM other` (the partition's rows give way to another table's);
/// - a `DELETE` of every row: `… MODIFY TTL …` and `… MATERIALIZE TTL` (rows already past the
///   new TTL expire), and the mutation `… DELETE WHERE 1` (a condition always true);
/// - an `UPDATE` of every row: `… CLEAR COLUMN | INDEX | PROJECTION …` (a column emptied in every
///   row), a column's TTL set with `… MODIFY COLUMN c … TTL …` (its expired values reset), and
///   the mutation `… UPDATE … WHERE 1`.
///
/// Other mutations (`… DELETE WHERE …`, `… UPDATE … WHERE …`) always have a `WHERE` (the server
/// requires it), like a `DELETE … WHERE`, and ask nothing. Words are read with sqlparser's
/// tokenizer, so a name or a string that says `drop` is not one; an open quote reads nothing.
fn clickhouse_alter_risk(sql: &str) -> Option<Risk> {
    let tokens: Vec<Token> = Tokenizer::new(&ClickHouseDialect {}, sql)
        .tokenize()
        .ok()?
        .into_iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect();
    let words: Vec<String> = tokens
        .iter()
        .map(|token| plain_word(token).unwrap_or_default())
        .collect();
    let has = |word: &str| words.iter().any(|w| w == word);
    let pair =
        |first: &str, second: &str| words.windows(2).any(|w| w[0] == first && w[1] == second);
    if has("drop") || (has("move") && pair("to", "table")) || pair("replace", "partition") {
        return Some(Risk::Drop);
    }
    // One `ALTER` may hold several commands, separated by commas outside brackets.
    let commands = clickhouse_alter_commands(&tokens);
    let mutation_of_every_row = |kind: &str| {
        commands
            .iter()
            .any(|command| clickhouse_mutation_of_every_row(&tokens[command.clone()], kind))
    };
    if pair("modify", "ttl") || pair("materialize", "ttl") || mutation_of_every_row("delete") {
        return Some(Risk::DeleteAll);
    }
    let column_ttl = commands.iter().any(|command| {
        let words = &words[command.clone()];
        words
            .windows(2)
            .any(|w| w[0] == "modify" && w[1] == "column")
            && words
                .iter()
                .enumerate()
                .any(|(i, w)| w == "ttl" && (i == 0 || words[i - 1] != "remove"))
    });
    if has("clear") || column_ttl || mutation_of_every_row("update") {
        return Some(Risk::UpdateAll);
    }
    None
}

/// The token ranges of an `ALTER`'s commands: split at its commas outside brackets.
fn clickhouse_alter_commands(tokens: &[Token]) -> Vec<std::ops::Range<usize>> {
    let mut commands = Vec::new();
    let mut depth = 0usize;
    let mut start = 0;
    for (i, token) in tokens.iter().enumerate() {
        match token {
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => depth = depth.saturating_sub(1),
            Token::Comma if depth == 0 => {
                commands.push(start..i);
                start = i + 1;
            }
            _ => {}
        }
    }
    commands.push(start..tokens.len());
    commands
}

/// Whether one command of an `ALTER` is the mutation `kind` (`delete` or `update`) with a
/// `WHERE` that is always true (`WHERE 1`, `WHERE true`, `WHERE 1 = 1`…), so every row changes.
/// The condition runs to the command's end, its `SETTINGS` or a `;`.
fn clickhouse_mutation_of_every_row(command: &[Token], kind: &str) -> bool {
    let mut depth = 0usize;
    let mut found_kind = false;
    let mut condition = None;
    for (i, token) in command.iter().enumerate() {
        match token {
            Token::LParen | Token::LBracket | Token::LBrace => depth += 1,
            Token::RParen | Token::RBracket | Token::RBrace => depth = depth.saturating_sub(1),
            _ if depth == 0 => match plain_word(token).as_deref() {
                Some(word) if word == kind => found_kind = true,
                Some("where") if found_kind => {
                    condition = Some(i + 1);
                    break;
                }
                _ => {}
            },
            _ => {}
        }
    }
    let Some(start) = condition else {
        return false;
    };
    let condition: Vec<String> = command[start..]
        .iter()
        .take_while(|token| {
            !matches!(token, Token::SemiColon | Token::EOF)
                && plain_word(token).as_deref() != Some("settings")
        })
        .map(Token::to_string)
        .collect();
    Parser::new(&ClickHouseDialect {})
        .try_with_sql(&condition.join(" "))
        .and_then(|mut parser| parser.parse_expr())
        .is_ok_and(|expr| always_true(&expr))
}

/// A condition true for every row whatever the row holds: a non-zero number, `true`, a literal
/// equal to itself, and those combined with `AND`, `OR` or brackets.
fn always_true(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            SqlValue::Boolean(b) => *b,
            SqlValue::Number(n, _) => n.parse::<f64>().is_ok_and(|n| n != 0.0),
            _ => false,
        },
        Expr::Nested(inner) => always_true(inner),
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::Or => always_true(left) || always_true(right),
            BinaryOperator::And => always_true(left) && always_true(right),
            BinaryOperator::Eq => match (left.as_ref(), right.as_ref()) {
                // `NULL = NULL` is not true.
                (Expr::Value(l), Expr::Value(r)) => l.value != SqlValue::Null && l.value == r.value,
                _ => false,
            },
            _ => false,
        },
        _ => false,
    }
}

/// The risk of a statement of HiveServer2's engines (Hive, Spark SQL, Impala) that replaces data
/// without saying `DELETE`, `TRUNCATE` or `DROP` first: `OVERWRITE` anywhere (`INSERT OVERWRITE
/// TABLE | DIRECTORY …`, `LOAD DATA … OVERWRITE INTO TABLE …`, Hive's `FROM … INSERT OVERWRITE`)
/// empties the table, the partition or the directory before writing, like a `TRUNCATE`; and
/// `ALTER TABLE … DROP` drops partitions (their rows) or columns. Words are read with sqlparser's
/// tokenizer, so a name or a string that says `overwrite` is not one; an open quote reads
/// nothing.
fn hive_risk(sql: &str) -> Option<Risk> {
    let tokens = Tokenizer::new(&HiveDialect {}, sql).tokenize().ok()?;
    let words: Vec<String> = tokens.iter().filter_map(plain_word).collect();
    if words.iter().any(|word| word == "overwrite") {
        Some(Risk::Truncate)
    } else if words.first().is_some_and(|word| word == "alter")
        && words.iter().any(|word| word == "drop")
    {
        Some(Risk::Drop)
    } else {
        None
    }
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
        "clickhouse" => Box::new(ClickHouseDialect {}),
        "oracle" => Box::new(OracleDialect {}),
        // libSQL reads SQL as SQLite does.
        "libsql" => Box::new(SQLiteDialect {}),
        // GoogleSQL, which BigQuery shares.
        "spanner" => Box::new(BigQueryDialect {}),
        // HiveServer2's engines: backquoted names, strings in both quotes.
        "hive" | "spark" | "kyuubi" | "impala" => Box::new(HiveDialect {}),
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
    use spelaion_dialect::SqlServerDialect;
    use spelaion_dialect::SqliteDialect;

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
        statement_risk("mysql", sql, None, safeguards)
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
            statement_risk("postgres", "/*!50000 drop table t */", None, &NONE),
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

    /// ClickHouse drops columns and partitions, and wipes columns, with `ALTER TABLE`.
    #[test]
    fn clickhouse_alter_table_drops_and_clears_ask() {
        let ch = |sql: &str| destructive("clickhouse", sql);
        assert_eq!(ch("alter table t drop column c"), Some(Risk::Drop));
        assert_eq!(ch("ALTER TABLE t DROP PARTITION 202401"), Some(Risk::Drop));
        assert_eq!(ch("alter table t drop index i"), Some(Risk::Drop));
        assert_eq!(ch("alter table t clear column c"), Some(Risk::UpdateAll));
        assert_eq!(ch("drop table t"), Some(Risk::Drop));
        assert_eq!(ch("truncate table t"), Some(Risk::Truncate));
        // Mutations always have their WHERE.
        assert_eq!(ch("alter table t delete where id = 1"), None);
        assert_eq!(ch("alter table t update a = 1 where id = 1"), None);
        assert_eq!(ch("alter table t add column c UInt8"), None);
        // A name or a string that says drop is not a DROP.
        assert_eq!(ch("alter table t comment column `drop` 'drop it'"), None);
        assert_eq!(ch("select 1"), None);
    }

    /// ClickHouse also takes data out of a table with `ALTER TABLE` by moving or replacing a
    /// partition, by a TTL, and by a mutation whose `WHERE` holds for every row: each asks as
    /// the closest risk.
    #[test]
    fn clickhouse_alter_table_moves_replaces_ttls_and_whole_mutations_ask() {
        let ch = |sql: &str| destructive("clickhouse", sql);
        // The partition leaves the table, or gives way to another table's rows.
        assert_eq!(
            ch("ALTER TABLE t MOVE PARTITION 202401 TO TABLE archive"),
            Some(Risk::Drop)
        );
        assert_eq!(
            ch("alter table t move partition id '202401' to table db.archive"),
            Some(Risk::Drop)
        );
        assert_eq!(
            ch("ALTER TABLE t REPLACE PARTITION 202401 FROM t2"),
            Some(Risk::Drop)
        );
        // Rows already past a new TTL expire.
        assert_eq!(
            ch("ALTER TABLE t MODIFY TTL d + INTERVAL 1 DAY"),
            Some(Risk::DeleteAll)
        );
        assert_eq!(ch("alter table t materialize ttl"), Some(Risk::DeleteAll));
        // A column's expired values reset to their default.
        assert_eq!(
            ch("alter table t modify column c String TTL d + interval 1 day"),
            Some(Risk::UpdateAll)
        );
        // Mutations over every row.
        assert_eq!(ch("ALTER TABLE t DELETE WHERE 1"), Some(Risk::DeleteAll));
        assert_eq!(
            ch("alter table t delete where true;"),
            Some(Risk::DeleteAll)
        );
        assert_eq!(
            ch("alter table t delete where (1 = 1)"),
            Some(Risk::DeleteAll)
        );
        assert_eq!(
            ch(
                "alter table t on cluster c delete in partition 1 where 1 settings mutations_sync = 2"
            ),
            Some(Risk::DeleteAll)
        );
        assert_eq!(
            ch("alter table t update a = 0 where 1"),
            Some(Risk::UpdateAll)
        );
        assert_eq!(
            ch("alter table t update a = 0 where id = 1, update b = 0 where 1 or id = 2"),
            Some(Risk::UpdateAll)
        );
        // Mutations whose WHERE picks rows, and moves and TTLs that keep the data.
        assert_eq!(ch("alter table t delete where 0"), None);
        assert_eq!(ch("alter table t delete where null = null"), None);
        assert_eq!(ch("alter table t delete where id = 1"), None);
        assert_eq!(ch("alter table t delete where a = 1 and 1"), None);
        assert_eq!(ch("alter table t update a = 1 where id in (1, 2)"), None);
        assert_eq!(
            ch("alter table t move partition 202401 to disk 'cold'"),
            None
        );
        assert_eq!(
            ch("alter table t move partition 202401 to volume 'slow'"),
            None
        );
        assert_eq!(ch("alter table t remove ttl"), None);
        assert_eq!(ch("alter table t modify column c remove ttl"), None);
        assert_eq!(ch("alter table t attach partition 202401 from t2"), None);
        assert_eq!(
            ch("alter table t modify column c String default replace(x, 'a', 'b')"),
            None
        );
        // A name or a string that holds those words is not one.
        assert_eq!(
            ch("alter table t comment column `move` 'move partition 1 to table x'"),
            None
        );
        assert_eq!(
            ch("alter table t comment column c 'replace partition 1 from t2, modify ttl d'"),
            None
        );
        assert_eq!(ch("alter table t modify column `ttl` UInt8"), None);
        assert_eq!(
            ch("alter table `replace` modify column `ttl` String comment 'delete where 1'"),
            None
        );
        assert_eq!(ch("alter table t update c = 'where 1' where id = 1"), None);
        assert_eq!(ch("alter table t delete where c = 'x where 1'"), None);
    }

    /// HiveServer2's engines replace data with `OVERWRITE` and drop partitions with `ALTER TABLE`;
    /// the rest is read as for every engine, with Hive's quoting.
    #[test]
    fn hive_overwrites_and_partition_drops_ask() {
        for engine in ["hive", "spark", "kyuubi", "impala"] {
            let hive = |sql: &str| destructive(engine, sql);
            assert_eq!(
                hive("insert overwrite table t select * from s"),
                Some(Risk::Truncate),
                "{engine}"
            );
            assert_eq!(
                hive("INSERT OVERWRITE TABLE t PARTITION (dt = '2026-01-01') SELECT 1"),
                Some(Risk::Truncate),
                "{engine}"
            );
            assert_eq!(
                hive("insert overwrite directory '/tmp/out' select * from t"),
                Some(Risk::Truncate),
                "{engine}"
            );
            assert_eq!(
                hive("load data inpath '/x' overwrite into table t"),
                Some(Risk::Truncate),
                "{engine}"
            );
            assert_eq!(
                hive("from s insert overwrite table t select a"),
                Some(Risk::Truncate),
                "{engine}"
            );
            assert_eq!(
                hive("alter table t drop partition (dt = '2026-01-01')"),
                Some(Risk::Drop),
                "{engine}"
            );
            assert_eq!(
                hive("alter table t drop if exists partition (p = 1)"),
                Some(Risk::Drop)
            );
            assert_eq!(
                hive("drop database d cascade"),
                Some(Risk::Drop),
                "{engine}"
            );
            assert_eq!(hive("truncate table t"), Some(Risk::Truncate), "{engine}");
            assert_eq!(hive("delete from t"), Some(Risk::DeleteAll), "{engine}");
            assert_eq!(hive("delete from `t` where id = 1"), None, "{engine}");
            // Appending, adding and reading ask nothing.
            assert_eq!(hive("insert into t select * from s"), None, "{engine}");
            assert_eq!(
                hive("alter table t add partition (p = 1)"),
                None,
                "{engine}"
            );
            assert_eq!(hive("select * from t"), None, "{engine}");
            // A name or a string that says overwrite or drop is not one.
            assert_eq!(
                hive("select `overwrite`, 'overwrite', \"drop\" from t"),
                None,
                "{engine}"
            );
            assert_eq!(hive("alter table t set tblproperties ('drop' = 'x')"), None);
        }
    }

    /// Oracle (ADR-059): what sqlparser reads is checked as for every engine; a PL/SQL block
    /// it cannot read asks when it holds a risky word or dynamic SQL, and defining a unit asks
    /// nothing, since nothing runs then.
    #[test]
    fn oracle_blocks_ask_for_what_they_run() {
        let ora = |sql: &str| destructive("oracle", sql);
        assert_eq!(ora("delete from t"), Some(Risk::DeleteAll));
        assert_eq!(ora("update t set a = 1"), Some(Risk::UpdateAll));
        assert_eq!(ora("truncate table t"), Some(Risk::Truncate));
        assert_eq!(ora("drop table t purge"), Some(Risk::Drop));
        assert_eq!(ora("delete from t where id = 1"), None);
        assert_eq!(ora("select * from t"), None);
        assert_eq!(
            ora("begin delete from t; end;"),
            Some(Risk::UncheckedWrite),
            "a DELETE in a block"
        );
        assert!(ora("declare n number; begin update t set a = 1; end;").is_some());
        assert_eq!(
            ora("begin execute immediate 'dr' || 'op table t'; end;"),
            Some(Risk::UncheckedWrite),
            "dynamic SQL cannot be read"
        );
        assert_eq!(
            ora("declare c integer := dbms_sql.open_cursor; begin null; end;"),
            Some(Risk::UncheckedWrite)
        );
        assert_eq!(ora("begin dbms_output.put_line('drop it'); end;"), None);
        assert_eq!(ora("begin null; end;"), None);
        // Defining a unit runs nothing.
        assert_eq!(
            ora("create or replace procedure p as begin delete from t; end;"),
            None
        );
    }
}
