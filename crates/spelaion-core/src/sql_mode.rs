//! The dialect a session splits its scripts with, as the session's `sql_mode` reads SQL text
//! (SDB-161). On MySQL and MariaDB, `NO_BACKSLASH_ESCAPES` makes `\` an ordinary character in
//! a string and `ANSI_QUOTES` makes `"…"` a name, so where a statement ends depends on the
//! session. The mode is read when the core opens a session (after its setup, ADR-037) and
//! again after a statement that names `sql_mode`; the next run splits as the session now
//! reads text. A script is split once, when it starts, as the `mysql` client does.
//!
//! Literals the core writes do not depend on it: `export::sql_literal` writes a MySQL string
//! that holds a backslash in a form every mode reads the same way.

use std::sync::{Arc, RwLock};

use futures_util::StreamExt;
use spelaion_driver_api::{Connection, Dialect, ExecEvent, ExecOptions, Value};

/// The driver's dialect, and the one for the session's current `sql_mode`.
pub(crate) struct SessionDialect {
    base: Arc<dyn Dialect>,
    current: RwLock<Arc<dyn Dialect>>,
    /// The dialect reads text differently by `sql_mode` (MySQL, MariaDB).
    follows_sql_mode: bool,
}

impl SessionDialect {
    pub(crate) fn new(base: Arc<dyn Dialect>) -> Self {
        let follows_sql_mode = base.with_sql_mode("").is_some();
        Self {
            current: RwLock::new(base.clone()),
            base,
            follows_sql_mode,
        }
    }

    pub(crate) fn current(&self) -> Arc<dyn Dialect> {
        self.current.read().expect("dialect lock poisoned").clone()
    }

    /// Reads the session's `sql_mode` and splits from now on as it reads text. A mode that
    /// cannot be read keeps the dialect as it was.
    pub(crate) async fn refresh(&self, connection: &dyn Connection) {
        if !self.follows_sql_mode {
            return;
        }
        let Some(mode) = read_sql_mode(connection).await else {
            return;
        };
        if let Some(dialect) = self.base.with_sql_mode(&mode) {
            *self.current.write().expect("dialect lock poisoned") = dialect;
        }
    }

    pub(crate) fn follows_sql_mode(&self) -> bool {
        self.follows_sql_mode
    }
}

/// How the session's `sql_mode` reads quoted text when it is not as the default mode reads
/// it (MySQL, MariaDB; SDB-262).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModeQuoting {
    /// `\` escapes inside a string: off under `NO_BACKSLASH_ESCAPES`.
    pub(crate) backslash_escapes: bool,
    /// `"…"` is a name: `ANSI_QUOTES` (or `ANSI`).
    pub(crate) ansi_quotes: bool,
}

impl ModeQuoting {
    /// From the dialect the session splits with. `None`: the default reading, or an engine the
    /// mode does not concern.
    pub(crate) fn of(dialect: &dyn Dialect) -> Option<ModeQuoting> {
        if dialect.name() != "mysql" {
            return None;
        }
        let mode = dialect.sql_mode()?;
        let has = |name: &str| mode.split(',').any(|m| m.trim().eq_ignore_ascii_case(name));
        let quoting = ModeQuoting {
            backslash_escapes: !has("NO_BACKSLASH_ESCAPES"),
            ansi_quotes: has("ANSI_QUOTES") || has("ANSI"),
        };
        (quoting.ansi_quotes || !quoting.backslash_escapes).then_some(quoting)
    }
}

/// Whether the session reads `\` in a string as an escape: as the default mode does, unless its
/// `sql_mode` has `NO_BACKSLASH_ESCAPES`.
pub(crate) fn backslash_escapes(dialect: &dyn Dialect) -> bool {
    ModeQuoting::of(dialect).is_none_or(|quoting| quoting.backslash_escapes)
}

/// `sql`, which the session reads under `quoting`, written so the default mode reads the same
/// statement: under `NO_BACKSLASH_ESCAPES` every `\` inside a string is doubled, and under
/// `ANSI_QUOTES` a `"…"` name becomes `` `…` ``. Comments are copied as they are, except
/// executable ones (`/*! … */`, `/*M! … */`), whose bodies are read as code. An unclosed quote
/// copies the rest as it is.
pub(crate) fn as_default_mode(sql: &str, quoting: ModeQuoting) -> String {
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len() + 8);
    let mut i = 0;
    // The end of a quoted text opened at `open` by `quote`: past the closing quote (a doubled
    // quote does not close it; a backslash escapes the next byte when `escapes`).
    let close = |open: usize, quote: u8, escapes: bool| {
        let mut j = open + 1;
        while j < b.len() {
            match b[j] {
                b'\\' if escapes => j += 2,
                c if c == quote && b.get(j + 1) == Some(&quote) => j += 2,
                c if c == quote => return Some(j + 1),
                _ => j += 1,
            }
        }
        None
    };
    while i < b.len() {
        let line_comment = b[i] == b'#'
            || (b[i] == b'-'
                && b.get(i + 1) == Some(&b'-')
                && b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace()));
        if line_comment {
            let end = b[i..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(b.len(), |p| i + p + 1);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let executable = b.get(i + 2) == Some(&b'!')
                || (b.get(i + 2).is_some_and(|c| c.eq_ignore_ascii_case(&b'm'))
                    && b.get(i + 3) == Some(&b'!'));
            if executable {
                // Its body is code: read on; its `*/` is copied like any other text.
                out.push_str("/*");
                i += 2;
                continue;
            }
            let end = sql[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            out.push_str(&sql[i..end]);
            i = end;
            continue;
        }
        match b[i] {
            quote @ (b'\'' | b'"') if !(quote == b'"' && quoting.ansi_quotes) => {
                let Some(end) = close(i, quote, quoting.backslash_escapes) else {
                    out.push_str(&sql[i..]);
                    break;
                };
                if quoting.backslash_escapes {
                    out.push_str(&sql[i..end]);
                } else {
                    // No escapes in the session: each backslash is a character of the string.
                    out.push_str(&sql[i..end].replace('\\', "\\\\"));
                }
                i = end;
            }
            b'"' => {
                // An ANSI_QUOTES name: `""` is a quote in it, and nothing else escapes.
                let Some(end) = close(i, b'"', false) else {
                    out.push_str(&sql[i..]);
                    break;
                };
                let name = sql[i + 1..end - 1].replace("\"\"", "\"");
                out.push('`');
                out.push_str(&name.replace('`', "``"));
                out.push('`');
                i = end;
            }
            b'`' => {
                let end = close(i, b'`', false).unwrap_or(b.len());
                out.push_str(&sql[i..end]);
                i = end;
            }
            _ => {
                let end = b[i + 1..]
                    .iter()
                    .position(|&c| matches!(c, b'#' | b'-' | b'/' | b'\'' | b'"' | b'`'))
                    .map_or(b.len(), |p| i + 1 + p);
                out.push_str(&sql[i..end]);
                i = end;
            }
        }
    }
    out
}

/// A statement that may change the session's `sql_mode` (`SET sql_mode = …`,
/// `SET @@SESSION.sql_mode = …`, `SET SESSION sql_mode …`): read it again afterwards.
pub(crate) fn may_change_sql_mode(statement: &str) -> bool {
    statement.to_ascii_lowercase().contains("sql_mode")
}

async fn read_sql_mode(connection: &dyn Connection) -> Option<String> {
    let mut events = connection
        .execute("select @@session.sql_mode", &ExecOptions::default())
        .await
        .ok()?;
    let mut mode = None;
    let mut logical = None;
    while let Some(event) = events.next().await {
        match event.ok()? {
            ExecEvent::Columns(schema) => logical = schema.columns.first().map(|c| c.logical),
            ExecEvent::Batch(batch) if mode.is_none() && batch.num_rows() > 0 => {
                mode = match Value::from_array(batch.column(0).as_ref(), 0, logical?)? {
                    Value::Null => Some(String::new()),
                    Value::Text(text) => Some(text),
                    other => Some(other.to_string()),
                };
            }
            _ => {}
        }
    }
    mode
}

#[cfg(test)]
mod transform_tests {
    use super::{ModeQuoting, as_default_mode};

    const NBE: ModeQuoting = ModeQuoting {
        backslash_escapes: false,
        ansi_quotes: false,
    };
    const ANSI: ModeQuoting = ModeQuoting {
        backslash_escapes: true,
        ansi_quotes: true,
    };
    const BOTH: ModeQuoting = ModeQuoting {
        backslash_escapes: false,
        ansi_quotes: true,
    };

    #[test]
    fn strings_and_names_are_rewritten_as_the_default_mode_reads_them() {
        // NO_BACKSLASH_ESCAPES: the string ends at its quote, its backslash is a character.
        assert_eq!(
            as_default_mode(r"update t set a = 'x\' -- ' where id = 1", NBE),
            r"update t set a = 'x\\' -- ' where id = 1"
        );
        assert_eq!(
            as_default_mode(r#"select "a\", 'it''s \ ok'"#, NBE),
            r#"select "a\\", 'it''s \\ ok'"#
        );
        // ANSI_QUOTES: `"…"` is a name; `'…'` still takes backslash escapes.
        assert_eq!(
            as_default_mode(r#"select "a\" , load_file('x\'y') -- ""#, ANSI),
            r#"select `a\` , load_file('x\'y') -- ""#
        );
        assert_eq!(
            as_default_mode(r#"select "we""ird`name" from t"#, ANSI),
            "select `we\"ird``name` from t"
        );
        // Both.
        assert_eq!(
            as_default_mode(r#"select "n\", 'v\'"#, BOTH),
            r#"select `n\`, 'v\\'"#
        );
    }

    #[test]
    fn comments_stay_and_executable_ones_are_read_as_code() {
        assert_eq!(
            as_default_mode("select 1 -- it's 'x\\\n# and 'y\\\n/* 'z\\ */", NBE),
            "select 1 -- it's 'x\\\n# and 'y\\\n/* 'z\\ */"
        );
        assert_eq!(
            as_default_mode(r"/*!50000 select 'a\' */", NBE),
            r"/*!50000 select 'a\\' */"
        );
        // `--` without a space is arithmetic, and an unclosed quote copies the rest.
        assert_eq!(
            as_default_mode("select 1--1, 'a\\", NBE),
            "select 1--1, 'a\\"
        );
        assert_eq!(
            as_default_mode("select `a\\`, 2/3", NBE),
            "select `a\\`, 2/3"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::may_change_sql_mode;

    #[test]
    fn statements_that_may_change_the_mode() {
        for sql in [
            "SET sql_mode = 'NO_BACKSLASH_ESCAPES'",
            "set @@session.SQL_MODE = concat(@@sql_mode, ',ANSI_QUOTES')",
            "SET SESSION sql_mode='ANSI'",
        ] {
            assert!(may_change_sql_mode(sql), "{sql}");
        }
        assert!(!may_change_sql_mode("set names utf8mb4"));
    }
}
