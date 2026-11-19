//! The data of a `COPY … FROM STDIN` in a script (SDB-162), as a plain `pg_dump` file writes
//! it: the rows on the lines after the statement, up to `\.`, which the dialect keeps apart
//! from the statements ([`Dialect::copy_data`]). The guard, the read-only checks, the
//! connection type's confirmation and the history only ever see the statement; its data is
//! never split, searched for placeholders or read as SQL.
//!
//! The rows are loaded through [`Connection::insert_rows`], the import's path (ADR-038): the
//! driver writes its own `COPY … FROM STDIN` for the table and columns the statement names and
//! sends the values as COPY data, so they reach a plugin as owned columns of text (ADR-041).
//! They go a chunk at a time, read from the script as they are sent, never held whole.
//! Without an open transaction they load in one of their own, so an error or a cancel half-way
//! leaves nothing loaded; inside the user's transaction they are part of it, as with `psql`.
//!
//! Only COPY's default text format is read (`\N` for NULL, tabs between values, backslash
//! escapes), which is what `pg_dump` writes. A statement with other options, or without its
//! column list, is refused before anything is sent.
//!
//! [`Dialect::copy_data`]: spelaion_driver_api::Dialect::copy_data

use futures_util::StreamExt;
use spelaion_driver_api::{
    Connection, DriverError, ExecEvent, ExecOptions, InsertTarget, TextRows, TransactionState,
};
use sqlparser::ast::{CopyLegacyOption, CopyOption, CopySource, CopyTarget, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;

/// Rows sent to the driver at a time.
const CHUNK_ROWS: usize = 10_000;
/// Bytes of data sent to the driver at a time, at most (about: a chunk ends after the row
/// that passes it).
const CHUNK_BYTES: usize = 8 * 1024 * 1024;

/// The table and columns `sql` (a `COPY … FROM STDIN`) loads, or why it cannot be loaded.
pub(crate) fn target_of(sql: &str) -> Result<InsertTarget, String> {
    let refused = |why: &str| format!("this COPY … FROM STDIN cannot be sent: {why}");
    let statements = Parser::parse_sql(&PostgreSqlDialect {}, sql)
        .map_err(|e| refused(&format!("it could not be read ({e})")))?;
    let [statement] = statements.as_slice() else {
        return Err(refused("it is not one statement"));
    };
    let Statement::Copy {
        source,
        to: false,
        target: CopyTarget::Stdin,
        options,
        legacy_options,
        ..
    } = statement
    else {
        return Err(refused("it is not a COPY … FROM STDIN"));
    };
    let CopySource::Table {
        table_name,
        columns,
    } = source
    else {
        return Err(refused("it does not name a table"));
    };
    if columns.is_empty() {
        return Err(refused(
            "name its columns, as pg_dump does: COPY t (a, b) FROM STDIN",
        ));
    }
    for option in options {
        let default = match option {
            CopyOption::Format(name) => name.value.eq_ignore_ascii_case("text"),
            CopyOption::Delimiter(c) => *c == '\t',
            CopyOption::Null(text) => text == "\\N",
            CopyOption::Encoding(name) => {
                matches!(name.to_ascii_uppercase().as_str(), "UTF8" | "UTF-8")
            }
            CopyOption::Freeze(_)
            | CopyOption::Header(_)
            | CopyOption::Quote(_)
            | CopyOption::Escape(_)
            | CopyOption::ForceQuote(_)
            | CopyOption::ForceNotNull(_)
            | CopyOption::ForceNull(_) => false,
        };
        if !default {
            return Err(refused(&format!(
                "only COPY's default text format is sent, and it asks for {option}"
            )));
        }
    }
    if let Some(option) = legacy_options.iter().find(|option| match option {
        CopyLegacyOption::Delimiter(c) => *c != '\t',
        CopyLegacyOption::Null(text) => text != "\\N",
        _ => true,
    }) {
        return Err(refused(&format!(
            "only COPY's default text format is sent, and it asks for {option}"
        )));
    }
    let parts: Vec<String> = table_name
        .0
        .iter()
        .map(|part| {
            part.as_ident()
                .map(unquoted)
                .ok_or_else(|| refused("its table name could not be read"))
        })
        .collect::<Result<_, _>>()?;
    let (schema, table) = match parts.as_slice() {
        [table] => (String::new(), table.clone()),
        [schema, table] => (schema.clone(), table.clone()),
        _ => return Err(refused("its table is named with more than a schema")),
    };
    Ok(InsertTarget {
        schema,
        table,
        columns: columns.iter().map(unquoted).collect(),
    })
}

/// A name as PostgreSQL reads it: a quoted one as written, an unquoted one in lower case.
fn unquoted(ident: &sqlparser::ast::Ident) -> String {
    match ident.quote_style {
        Some(_) => ident.value.clone(),
        None => ident.value.to_lowercase(),
    }
}

/// One value of COPY's text format: `\N` is NULL; backslash escapes are decoded (`\t`, `\n`,
/// `\r`, `\b`, `\f`, `\v`, up to three octal digits, `\x` with one or two hex digits, and a
/// backslash before any other character stands for that character).
pub(crate) fn decode_value(field: &str) -> Result<Option<String>, String> {
    if field == "\\N" {
        return Ok(None);
    }
    if !field.contains('\\') {
        return Ok(Some(field.to_string()));
    }
    let b = field.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            out.push(b[i]);
            i += 1;
            continue;
        }
        let Some(&c) = b.get(i + 1) else {
            return Err("a value ends with a lone backslash".into());
        };
        i += 2;
        match c {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0b),
            b'0'..=b'7' => {
                let mut value = u32::from(c - b'0');
                for _ in 0..2 {
                    match b.get(i) {
                        Some(&d @ b'0'..=b'7') => {
                            value = value * 8 + u32::from(d - b'0');
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push((value & 0xff) as u8);
            }
            b'x' if b.get(i).is_some_and(u8::is_ascii_hexdigit) => {
                let mut value = 0u32;
                for _ in 0..2 {
                    match b.get(i) {
                        Some(&d) if d.is_ascii_hexdigit() => {
                            value = value * 16 + (d as char).to_digit(16).unwrap_or(0);
                            i += 1;
                        }
                        _ => break,
                    }
                }
                out.push(value as u8);
            }
            other => out.push(other),
        }
    }
    String::from_utf8(out)
        .map(Some)
        .map_err(|_| "a value is not valid UTF-8 once its escapes are read".into())
}

/// Runs `sql`, a control statement, to its end.
async fn control(
    connection: &dyn Connection,
    sql: &str,
    options: &ExecOptions,
) -> Result<(), DriverError> {
    let mut events = connection.execute(sql, options).await?;
    while let Some(event) = events.next().await {
        match event? {
            ExecEvent::Columns(_)
            | ExecEvent::Batch(_)
            | ExecEvent::Notice(_)
            | ExecEvent::Done { .. } => {}
        }
    }
    Ok(())
}

/// Loads the rows of `data` into the table `sql` names; the number of rows loaded. `cancelled`
/// is checked before each chunk (a chunk in flight is stopped by the driver's own cancel).
pub(crate) async fn load(
    connection: &dyn Connection,
    sql: &str,
    data: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
    options: &ExecOptions,
) -> Result<u64, DriverError> {
    if !connection.capabilities().insert_rows {
        return Err(DriverError::Other(
            "this connection cannot load the data of COPY … FROM STDIN".into(),
        ));
    }
    let target = target_of(sql).map_err(DriverError::Other)?;
    // Its own transaction when none is open: all or nothing.
    let own = matches!(
        connection.transaction_state().await?,
        TransactionState::Idle
    );
    if own {
        control(connection, "BEGIN", options).await?;
    }
    let loaded = send(connection, &target, data, cancelled).await;
    if own {
        match &loaded {
            Ok(_) => control(connection, "COMMIT", options).await?,
            // Best effort: closing the session rolls it back anyway.
            Err(_) => {
                let _ = control(connection, "ROLLBACK", options).await;
            }
        }
    }
    loaded
}

async fn send(
    connection: &dyn Connection,
    target: &InsertTarget,
    data: &str,
    cancelled: &(dyn Fn() -> bool + Sync),
) -> Result<u64, DriverError> {
    let width = target.columns.len();
    let mut total = 0u64;
    let mut builder = TextRows::builder(width);
    let mut rows = 0usize;
    let mut bytes = 0usize;
    let flush = |builder: TextRows| async move {
        if cancelled() {
            return Err(DriverError::Cancelled);
        }
        connection.insert_rows(target, builder).await
    };
    // `data` ends with a newline after its last row (or is empty).
    let body = data.strip_suffix('\n').unwrap_or(data);
    if !data.is_empty() {
        for (number, line) in body.split('\n').enumerate() {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut values = Vec::with_capacity(width);
            for field in line.split('\t') {
                values.push(decode_value(field).map_err(|why| {
                    DriverError::Other(format!("line {} of the COPY data: {why}", number + 1))
                })?);
            }
            if values.len() != width {
                return Err(DriverError::Other(format!(
                    "line {} of the COPY data has {} values, and the COPY names {width} columns",
                    number + 1,
                    values.len()
                )));
            }
            builder
                .push(values.iter().map(Option::as_deref))
                .map_err(|e| DriverError::Other(e.to_string()))?;
            rows += 1;
            bytes += line.len();
            if rows >= CHUNK_ROWS || bytes >= CHUNK_BYTES {
                let full = std::mem::replace(&mut builder, TextRows::builder(width)).finish();
                total += flush(full).await?;
                rows = 0;
                bytes = 0;
            }
        }
    }
    if rows > 0 {
        total += flush(builder.finish()).await?;
    } else if cancelled() {
        return Err(DriverError::Cancelled);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_is_the_table_and_columns_named() {
        let target = target_of("COPY public.t (id, \"Note\", Amount) FROM stdin").unwrap();
        assert_eq!(target.schema, "public");
        assert_eq!(target.table, "t");
        assert_eq!(target.columns, ["id", "Note", "amount"]);
        let target =
            target_of("copy \"My Table\" (a) from stdin with (format text, delimiter E'\\t')")
                .unwrap();
        assert_eq!(
            (target.schema.as_str(), target.table.as_str()),
            ("", "My Table")
        );
    }

    #[test]
    fn anything_but_the_default_text_format_is_refused() {
        for sql in [
            "copy t from stdin",
            "copy t (a) from stdin with (format csv)",
            "copy t (a) from stdin with (format binary)",
            "copy t (a) from stdin with (delimiter ',')",
            "copy t (a) from stdin with (null '')",
            "copy t (a) from stdin with (header)",
            "copy t (a) from stdin csv",
            "copy t (a) to stdout",
            "copy t (a) from '/tmp/x'",
            "copy (select 1) to stdout",
        ] {
            assert!(target_of(sql).is_err(), "{sql}");
        }
    }

    #[test]
    fn copy_text_values_are_decoded() {
        assert_eq!(decode_value("\\N").unwrap(), None);
        assert_eq!(decode_value("").unwrap().as_deref(), Some(""));
        assert_eq!(decode_value("plain").unwrap().as_deref(), Some("plain"));
        assert_eq!(
            decode_value("a\\tb\\nc\\\\d\\re\\bf\\fg\\vh")
                .unwrap()
                .as_deref(),
            Some("a\tb\nc\\d\re\u{8}f\u{c}g\u{b}h")
        );
        // A bytea as pg_dump writes it: its own backslash escaped.
        assert_eq!(
            decode_value("\\\\x0aff").unwrap().as_deref(),
            Some("\\x0aff")
        );
        assert_eq!(
            decode_value("\\101\\x42\\x4").unwrap().as_deref(),
            Some("AB\u{4}")
        );
        // A backslash before any other character stands for it; `\N` inside a value is N.
        assert_eq!(decode_value("x\\Ny\\.").unwrap().as_deref(), Some("xNy."));
        assert!(decode_value("end\\").is_err());
        assert!(decode_value("\\377").is_err(), "not UTF-8");
        assert_eq!(decode_value("é ✓").unwrap().as_deref(), Some("é ✓"));
    }
}
