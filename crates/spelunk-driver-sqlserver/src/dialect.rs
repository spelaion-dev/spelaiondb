use spelunk_driver_api::{Dialect, StatementSpan};

/// SQL Server (T-SQL) dialect.
///
/// T-SQL does not need `;` between statements and wraps bodies in `BEGIN … END` without any
/// quoting, so the splitter:
/// - ends a batch at a line that is only `GO` (the line is not sent);
/// - splits at `;` only outside quotes, comments and `BEGIN … END` / `CASE … END` blocks;
/// - keeps the rest of the batch as one statement after `CREATE`/`ALTER` of a procedure,
///   function, trigger or view, which must be alone in their batch anyway.
///
/// Several statements without `;` between them go to the server as one batch: SQL Server runs
/// them in order and each result set is shown.
pub struct SqlServerDialect;

impl Dialect for SqlServerDialect {
    fn name(&self) -> &'static str {
        "sqlserver"
    }

    fn split_statements(&self, script: &str) -> Vec<StatementSpan> {
        split(script)
    }

    fn quote_identifier(&self, ident: &str) -> String {
        let simple = ident
            .bytes()
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && ident
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            && !RESERVED.contains(&ident.to_ascii_lowercase().as_str());
        if simple {
            ident.to_string()
        } else {
            format!("[{}]", ident.replace(']', "]]"))
        }
    }
}

/// T-SQL reserved keywords (they cannot be used unquoted as identifiers).
const RESERVED: &[&str] = &[
    "add",
    "all",
    "alter",
    "and",
    "any",
    "as",
    "asc",
    "authorization",
    "backup",
    "begin",
    "between",
    "break",
    "browse",
    "bulk",
    "by",
    "cascade",
    "case",
    "check",
    "checkpoint",
    "close",
    "clustered",
    "coalesce",
    "collate",
    "column",
    "commit",
    "compute",
    "constraint",
    "contains",
    "containstable",
    "continue",
    "convert",
    "create",
    "cross",
    "current",
    "current_date",
    "current_time",
    "current_timestamp",
    "current_user",
    "cursor",
    "database",
    "dbcc",
    "deallocate",
    "declare",
    "default",
    "delete",
    "deny",
    "desc",
    "disk",
    "distinct",
    "distributed",
    "double",
    "drop",
    "dump",
    "else",
    "end",
    "errlvl",
    "escape",
    "except",
    "exec",
    "execute",
    "exists",
    "exit",
    "external",
    "fetch",
    "file",
    "fillfactor",
    "for",
    "foreign",
    "freetext",
    "freetexttable",
    "from",
    "full",
    "function",
    "goto",
    "grant",
    "group",
    "having",
    "holdlock",
    "identity",
    "identity_insert",
    "identitycol",
    "if",
    "in",
    "index",
    "inner",
    "insert",
    "intersect",
    "into",
    "is",
    "join",
    "key",
    "kill",
    "left",
    "like",
    "lineno",
    "load",
    "merge",
    "national",
    "nocheck",
    "nonclustered",
    "not",
    "null",
    "nullif",
    "of",
    "off",
    "offsets",
    "on",
    "open",
    "opendatasource",
    "openquery",
    "openrowset",
    "openxml",
    "option",
    "or",
    "order",
    "outer",
    "over",
    "percent",
    "pivot",
    "plan",
    "precision",
    "primary",
    "print",
    "proc",
    "procedure",
    "public",
    "raiserror",
    "read",
    "readtext",
    "reconfigure",
    "references",
    "replication",
    "restore",
    "restrict",
    "return",
    "revert",
    "revoke",
    "right",
    "rollback",
    "rowcount",
    "rowguidcol",
    "rule",
    "save",
    "schema",
    "securityaudit",
    "select",
    "semantickeyphrasetable",
    "semanticsimilaritydetailstable",
    "semanticsimilaritytable",
    "session_user",
    "set",
    "setuser",
    "shutdown",
    "some",
    "statistics",
    "system_user",
    "table",
    "tablesample",
    "textsize",
    "then",
    "to",
    "top",
    "tran",
    "transaction",
    "trigger",
    "truncate",
    "try_convert",
    "tsequal",
    "union",
    "unique",
    "unpivot",
    "update",
    "updatetext",
    "use",
    "user",
    "values",
    "varying",
    "view",
    "waitfor",
    "when",
    "where",
    "while",
    "with",
    "within",
    "writetext",
];

/// Statements whose batch has a row count worth reporting when they return no rows.
pub fn counts_rows(sql: &str) -> bool {
    let mut rest = sql.trim_start();
    // Leading comments.
    loop {
        if let Some(after) = rest.strip_prefix("--") {
            rest = after.split_once('\n').map_or("", |(_, r)| r).trim_start();
        } else if let Some(after) = rest.strip_prefix("/*") {
            rest = after.split_once("*/").map_or("", |(_, r)| r).trim_start();
        } else {
            break;
        }
    }
    let first: String = rest.chars().take_while(char::is_ascii_alphabetic).collect();
    matches!(
        first.to_ascii_lowercase().as_str(),
        "insert" | "update" | "delete" | "merge" | "with"
    )
}

fn is_word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'@' || c == b'#' || c == b'$' || c >= 0x80
}

/// Is the line starting at `start` only `GO` (optionally with a count and a `--` comment)?
/// Returns where the line ends.
fn go_line(b: &[u8], start: usize) -> Option<usize> {
    let end = b[start..]
        .iter()
        .position(|&c| c == b'\n')
        .map_or(b.len(), |p| start + p);
    let line = std::str::from_utf8(&b[start..end]).ok()?;
    let line = line.split("--").next().unwrap_or("").trim();
    let mut words = line.split_whitespace();
    let first = words.next()?;
    let count_ok = words
        .next()
        .is_none_or(|n| n.bytes().all(|c| c.is_ascii_digit()));
    (first.eq_ignore_ascii_case("go") && count_ok && words.next().is_none()).then_some(end)
}

/// Statements that must be alone in their batch: the rest of the batch belongs to them.
fn takes_rest_of_batch(words: &[String]) -> bool {
    let mut rest = words.iter().map(String::as_str);
    let Some(first) = rest.next() else {
        return false;
    };
    if first != "create" && first != "alter" {
        return false;
    }
    let mut next = rest.next();
    if first == "create" && next == Some("or") {
        rest.next(); // `alter`
        next = rest.next();
    }
    matches!(
        next,
        Some("procedure" | "proc" | "function" | "trigger" | "view")
    )
}

fn split(script: &str) -> Vec<StatementSpan> {
    let b = script.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut has_code = false;
    // The first words of the current statement, lowercased.
    let mut words: Vec<String> = Vec::new();
    let mut depth = 0usize;
    let mut rest_of_batch = false;
    let mut at_line_start = true;
    let mut i = 0;

    let push = |spans: &mut Vec<StatementSpan>, from: usize, to: usize| {
        let text = &script[from..to];
        let lead = text.len() - text.trim_start().len();
        let trail = text.len() - text.trim_end().len();
        spans.push(StatementSpan {
            start: from + lead,
            end: to - trail,
        });
    };

    while i < b.len() {
        if at_line_start {
            let indent = b[i..]
                .iter()
                .take_while(|c| **c == b' ' || **c == b'\t')
                .count();
            if let Some(end) = go_line(b, i + indent) {
                if has_code {
                    push(&mut spans, start, i);
                }
                i = end;
                start = i;
                has_code = false;
                words.clear();
                depth = 0;
                rest_of_batch = false;
                continue;
            }
            at_line_start = false;
        }
        match b[i] {
            b'\n' => {
                at_line_start = true;
                i += 1;
            }
            b'\'' => {
                i = skip_quoted(b, i, b'\'', b'\'');
                has_code = true;
            }
            b'"' => {
                i = skip_quoted(b, i, b'"', b'"');
                has_code = true;
            }
            b'[' => {
                i = skip_quoted(b, i, b'[', b']');
                has_code = true;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                // Block comments nest in T-SQL.
                let mut nested = 0usize;
                while i < b.len() {
                    if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                        nested += 1;
                        i += 2;
                    } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                        nested -= 1;
                        i += 2;
                        if nested == 0 {
                            break;
                        }
                    } else {
                        if b[i] == b'\n' {
                            at_line_start = false;
                        }
                        i += 1;
                    }
                }
            }
            b';' if depth == 0 && !rest_of_batch => {
                if has_code {
                    push(&mut spans, start, i);
                }
                i += 1;
                start = i;
                has_code = false;
                words.clear();
            }
            c if is_word_byte(c) && (i == 0 || !is_word_byte(b[i - 1])) => {
                let end = b[i..]
                    .iter()
                    .position(|&c| !is_word_byte(c))
                    .map_or(b.len(), |p| i + p);
                let word = script[i..end].to_ascii_lowercase();
                has_code = true;
                if words.len() < 4 {
                    words.push(word.clone());
                    if takes_rest_of_batch(&words) {
                        rest_of_batch = true;
                    }
                }
                match word.as_str() {
                    // `BEGIN TRAN`, `BEGIN DISTRIBUTED`, `BEGIN DIALOG` are not blocks.
                    "begin" => {
                        let next = next_word(script, end);
                        if !matches!(
                            next.as_str(),
                            "tran" | "transaction" | "distributed" | "dialog" | "conversation"
                        ) {
                            depth += 1;
                        }
                    }
                    "case" => depth += 1,
                    // `END CONVERSATION` is a statement, not the end of a block.
                    "end" if next_word(script, end) != "conversation" => {
                        depth = depth.saturating_sub(1)
                    }
                    _ => {}
                }
                i = end;
            }
            c => {
                if !c.is_ascii_whitespace() {
                    has_code = true;
                }
                i += 1;
            }
        }
    }
    if has_code {
        push(&mut spans, start, b.len());
    }
    spans
}

/// The next word after `from`, lowercased (empty at the end).
fn next_word(script: &str, from: usize) -> String {
    script[from..]
        .trim_start()
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// Skips a quoted text starting at `i` (the opening character); a doubled closing character
/// is an escaped one.
fn skip_quoted(b: &[u8], i: usize, _open: u8, close: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == close {
            if b.get(j + 1) == Some(&close) {
                j += 2;
            } else {
                return j + 1;
            }
        } else {
            j += 1;
        }
    }
    b.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts(script: &str) -> Vec<&str> {
        split(script).iter().map(|s| s.text(script)).collect()
    }

    #[test]
    fn splits_at_semicolons_and_go_lines() {
        assert_eq!(
            parts("select 1; select 2\nGO\nselect 3\n  go 2 -- twice\nselect 4"),
            ["select 1", "select 2", "select 3", "select 4"]
        );
        // `go` inside a line is not a separator.
        assert_eq!(parts("select 1 as go"), ["select 1 as go"]);
    }

    #[test]
    fn respects_quotes_brackets_and_comments() {
        let script = "select 'a;b', \"c;d\", [e;]]f] -- g;h\n; /* i; /* j; */ k; */ select 2";
        assert_eq!(
            parts(script),
            [
                "select 'a;b', \"c;d\", [e;]]f] -- g;h",
                "/* i; /* j; */ k; */ select 2"
            ]
        );
    }

    #[test]
    fn blocks_are_not_split() {
        let script = "if 1 = 1 begin print 'a'; print 'b'; end; \
                      begin try select 1/0; end try begin catch select 2; end catch; \
                      select case when 1 = 1 then 'x' end; begin tran; commit";
        assert_eq!(
            parts(script),
            [
                "if 1 = 1 begin print 'a'; print 'b'; end",
                "begin try select 1/0; end try begin catch select 2; end catch",
                "select case when 1 = 1 then 'x' end",
                "begin tran",
                "commit"
            ]
        );
    }

    #[test]
    fn procedures_keep_the_rest_of_their_batch() {
        let script =
            "create or alter procedure p as\nset nocount on;\nselect 1;\nselect 2;\ngo\nexec p";
        assert_eq!(
            parts(script),
            [
                "create or alter procedure p as\nset nocount on;\nselect 1;\nselect 2;",
                "exec p"
            ]
        );
        assert_eq!(
            parts("create view v as select 1 as a; select 2"),
            ["create view v as select 1 as a; select 2"]
        );
        // Other CREATE statements split as usual.
        assert_eq!(
            parts("create table t (a int); select 2"),
            ["create table t (a int)", "select 2"]
        );
    }

    #[test]
    fn row_counts_only_for_writes() {
        assert!(counts_rows("-- note\n/* x */ UPDATE t set a = 1"));
        assert!(counts_rows("with x as (select 1 as a) delete from t"));
        assert!(!counts_rows("create table t (a int)"));
        assert!(!counts_rows("exec p"));
    }

    #[test]
    fn quotes_identifiers_only_when_needed() {
        let d = SqlServerDialect;
        assert_eq!(d.quote_identifier("orders"), "orders");
        assert_eq!(d.quote_identifier("Orders"), "Orders");
        assert_eq!(d.quote_identifier("user"), "[user]");
        assert_eq!(d.quote_identifier("order lines"), "[order lines]");
        assert_eq!(d.quote_identifier("a]b"), "[a]]b]");
    }
}
