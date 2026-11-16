//! A folder of CSV, TSV and Parquet files as a DuckDB connection (SDB-316).
//!
//! The connection is an in-memory DuckDB database, always read-only (the resolver sets
//! `read_only`, so the session runs only reads, ADR-053). Before anything of the user's runs,
//! its session is set up here, on the driver's connection itself:
//!
//! 1. `allowed_directories` is the folder alone (with its trailing separator, so a sibling
//!    whose name starts with the folder's is not inside it), and `enable_external_access` is
//!    turned off: DuckDB then opens no file outside the folder, no other database, no extension
//!    and nothing on the network.
//! 2. The folder's files are listed by DuckDB's own `glob`, and each CSV, TSV or Parquet file
//!    becomes a view named after it, quoted by the dialect; its path is a string literal. A
//!    file DuckDB cannot read gets no view and is reported.
//! 3. `lock_configuration` is set: no statement can change any of it afterwards.
//!
//! The files are untrusted input read by DuckDB. Nothing is ever written to the folder: the
//! only statements that could (`COPY … TO`, `EXPORT DATABASE`) are not reads, and the session's
//! read-only barrier refuses them before they reach DuckDB.

use std::collections::HashSet;
use std::path::Path;

use futures_util::StreamExt;
use spelaion_driver_api::{Connection, Dialect, ExecEvent, ExecOptions, Value};

/// Views made for one folder at most: a folder with more files keeps the first ones by name.
pub const MAX_VIEWS: usize = 1000;

/// What setting up a folder made of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FolderReport {
    /// The views made, by name, each with the file it reads.
    pub views: Vec<FolderView>,
    /// Files of the folder that got no view, with why.
    pub skipped: Vec<SkippedFile>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderView {
    pub name: String,
    pub file: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedFile {
    pub file: String,
    pub reason: String,
}

/// How DuckDB reads a file, by its name; `None` for a file the folder does not show.
fn reader(name: &str) -> Option<(&'static str, &'static str)> {
    let lower = name.to_lowercase();
    // (extension, the reader's call before the path, after it)
    const READERS: [(&str, &str, &str); 5] = [
        (".csv", "read_csv(", ")"),
        (".csv.gz", "read_csv(", ")"),
        (".tsv", "read_csv(", ", delim = '\t')"),
        (".tsv.gz", "read_csv(", ", delim = '\t')"),
        (".parquet", "read_parquet(", ")"),
    ];
    READERS
        .iter()
        .filter(|(extension, ..)| lower.ends_with(extension))
        .map(|(_, call, rest)| (*call, *rest))
        .next()
}

/// The extension `reader` matched, to take it off the view's name.
fn stem(name: &str) -> &str {
    let lower = name.to_lowercase();
    for extension in [".csv.gz", ".tsv.gz", ".parquet", ".csv", ".tsv"] {
        if lower.ends_with(extension) {
            return &name[..name.len() - extension.len()];
        }
    }
    name
}

/// Characters DuckDB reads as a pattern in a path: a path with one names other files.
fn has_pattern(text: &str) -> bool {
    text.contains(['*', '?', '['])
}

/// A SQL string literal: single quotes, doubled inside.
fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// The folder as DuckDB is given it: its text, with `/` separators on Windows and a trailing
/// separator, checked to be something DuckDB reads as that folder and nothing else.
fn folder_text(folder: &Path) -> Result<String, String> {
    let shown = folder.display();
    let text = folder
        .to_str()
        .ok_or_else(|| format!("the folder {shown} has a name that is not valid UTF-8"))?;
    if text.contains('\0') {
        return Err(format!(
            "the folder {shown} has a NUL character in its name"
        ));
    }
    if has_pattern(text) {
        return Err(format!(
            "the folder {shown} has *, ? or [ in its path, which DuckDB reads as a pattern: move or rename it"
        ));
    }
    let mut text = if cfg!(windows) {
        text.replace('\\', "/")
    } else {
        text.to_string()
    };
    if !text.ends_with('/') {
        text.push('/');
    }
    Ok(text)
}

/// Sets up `connection`, a new in-memory DuckDB session, to show `folder` and read nothing
/// else. Any failure before the configuration is locked fails the whole connection: the caller
/// closes it.
pub(crate) async fn prepare(
    connection: &dyn Connection,
    dialect: &dyn Dialect,
    folder: &Path,
) -> Result<FolderReport, String> {
    if !folder.is_dir() {
        return Err(format!("{} is not a folder", folder.display()));
    }
    let dir = folder_text(folder)?;
    run(
        connection,
        &format!("set allowed_directories = [{}]", literal(&dir)),
    )
    .await?;
    run(connection, "set enable_external_access = false").await?;

    let files = first_column(
        connection,
        &format!(
            "select file from glob({}) order by file",
            literal(&format!("{dir}*"))
        ),
    )
    .await?;
    let mut report = FolderReport::default();
    // DuckDB's names are not case-sensitive: two views may not differ only in case.
    let mut taken = HashSet::new();
    for path in files {
        let file = path
            .strip_prefix(&dir)
            .unwrap_or_else(|| path.rsplit(['/', '\\']).next().unwrap_or(&path))
            .to_string();
        let Some((call, rest)) = reader(&file) else {
            continue;
        };
        let skip = |reason: &str| SkippedFile {
            file: file.clone(),
            reason: reason.to_string(),
        };
        if has_pattern(&file) {
            report.skipped.push(skip(
                "its name has *, ? or [, which DuckDB reads as a pattern",
            ));
            continue;
        }
        if report.views.len() >= MAX_VIEWS {
            report.skipped.push(skip(&format!(
                "the folder has more than {MAX_VIEWS} files: only the first ones by name are shown"
            )));
            continue;
        }
        let name = [stem(&file), file.as_str()]
            .into_iter()
            .find(|name| !name.is_empty() && !taken.contains(&name.to_lowercase()))
            .map(str::to_string);
        let Some(name) = name else {
            report
                .skipped
                .push(skip("another file already gives a view that name"));
            continue;
        };
        let sql = format!(
            "create view {} as select * from {call}{}{rest}",
            dialect.quote_identifier(&name),
            literal(&format!("{dir}{file}"))
        );
        match run(connection, &sql).await {
            Ok(()) => {
                taken.insert(name.to_lowercase());
                report.views.push(FolderView { name, file });
            }
            Err(reason) => report.skipped.push(skip(&reason)),
        }
    }

    run(connection, "set lock_configuration = true").await?;
    Ok(report)
}

/// Runs one statement and reads it to its end.
async fn run(connection: &dyn Connection, sql: &str) -> Result<(), String> {
    let mut events = connection
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    while let Some(event) = events.next().await {
        event.map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The first column of every row of a query, as text.
async fn first_column(connection: &dyn Connection, sql: &str) -> Result<Vec<String>, String> {
    let mut events = connection
        .execute(sql, &ExecOptions::default())
        .await
        .map_err(|e| e.to_string())?;
    let mut logical = None;
    let mut out = Vec::new();
    while let Some(event) = events.next().await {
        match event.map_err(|e| e.to_string())? {
            ExecEvent::Columns(schema) => logical = schema.columns.first().map(|c| c.logical),
            ExecEvent::Batch(batch) if batch.num_columns() > 0 => {
                let Some(logical) = logical else { continue };
                for row in 0..batch.num_rows() {
                    match Value::from_array(batch.column(0).as_ref(), row, logical) {
                        Some(Value::Text(text)) => out.push(text),
                        Some(Value::Null) | None => {}
                        Some(other) => out.push(other.to_string()),
                    }
                }
            }
            ExecEvent::Batch(_) | ExecEvent::Done { .. } | ExecEvent::Notice(_) => {}
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_read_by_their_extension_and_named_without_it() {
        assert_eq!(reader("orders.csv"), Some(("read_csv(", ")")));
        assert_eq!(reader("Orders.CSV.GZ"), Some(("read_csv(", ")")));
        assert_eq!(reader("lines.tsv"), Some(("read_csv(", ", delim = '\t')")));
        assert_eq!(reader("events.parquet"), Some(("read_parquet(", ")")));
        assert_eq!(reader("notes.txt"), None);
        assert_eq!(reader("archive.gz"), None);
        assert_eq!(stem("Orders.CSV.GZ"), "Orders");
        assert_eq!(stem("events.parquet"), "events");
        assert_eq!(stem(".csv"), "");
    }

    #[test]
    fn a_folder_is_given_with_a_trailing_separator_and_never_as_a_pattern() {
        let folder = std::env::temp_dir().join("sales");
        let text = folder_text(&folder).unwrap();
        assert!(text.ends_with("sales/"), "{text}");
        if cfg!(windows) {
            assert!(!text.contains('\\'), "{text}");
        }
        for name in ["sales*", "sales?", "sales[1]"] {
            let error = folder_text(&std::env::temp_dir().join(name)).unwrap_err();
            assert!(error.contains("pattern"), "{error}");
        }
    }

    #[test]
    fn literals_double_their_quotes() {
        assert_eq!(literal("C:/it's/"), "'C:/it''s/'");
    }
}
