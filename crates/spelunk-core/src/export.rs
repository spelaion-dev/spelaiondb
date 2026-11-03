//! Results written as text formats: to copy a selection to the clipboard or to export a whole
//! result to a file. The rows come from the result store (ADR-010), so typed data is used:
//! numbers stay numbers in JSON, NULL is `NULL` in SQL and literals follow the engine.

use std::io::{self, Write};
use std::ops::Range;

use spelunk_driver_api::{Dialect, LogicalType, Value};

use crate::results::StoredResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportFormat {
    /// Tab-separated, for pasting into spreadsheets. NULL is an empty cell.
    Tsv,
    /// RFC 4180 (comma, `"` quoting, CRLF). NULL is an empty field.
    Csv,
    /// Array of objects, one per row, keyed by column name.
    Json,
    /// One `INSERT` per row into `table`, with the engine's literals and quoting.
    SqlInsert { table: String },
    /// A Markdown (GitHub) table: always with the header row, numbers aligned right.
    Markdown,
    /// The values as an SQL list for `IN`: `(1, 'a')`, or `((1, 'a'), (2, 'b'))` with several
    /// columns; duplicates kept once, in order.
    InList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    pub format: ExportFormat,
    /// Rows of the result (clipped at its end).
    pub rows: Range<usize>,
    /// Columns of the result, by position (clipped).
    pub columns: Range<usize>,
    /// Column names as the first line (TSV/CSV). JSON and SQL always name the columns.
    pub header: bool,
}

/// For results whose connection is gone: standard SQL quoting (`"x"`) and literals.
pub struct GenericDialect;

impl Dialect for GenericDialect {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn split_statements(&self, _script: &str) -> Vec<spelunk_driver_api::StatementSpan> {
        Vec::new()
    }

    fn quote_identifier(&self, ident: &str) -> String {
        format!("\"{}\"", ident.replace('"', "\"\""))
    }
}

/// Rows read per batch from the store while writing (keeps memory bounded on big exports).
const CHUNK: usize = 10_000;

/// Writes the selected rows and columns of `result` to `out`; returns how many rows.
pub fn write_result(
    result: &StoredResult,
    options: &ExportOptions,
    dialect: &dyn Dialect,
    out: &mut dyn Write,
) -> io::Result<usize> {
    let schema = result.schema();
    let total_columns = schema.columns.len();
    let columns = options.columns.start.min(total_columns)..options.columns.end.min(total_columns);
    let rows = options.rows.start.min(result.rows())..options.rows.end.min(result.rows());
    let metas = &schema.columns[columns.clone()];
    let names: Vec<String> = unique_names(metas.iter().map(|m| m.name.as_str()));

    match &options.format {
        ExportFormat::Tsv | ExportFormat::Csv if options.header => {
            let fields: Vec<String> = names
                .iter()
                .map(|n| field(&options.format, Some(n)))
                .collect();
            write_line(out, &options.format, &fields)?;
        }
        ExportFormat::Json => out.write_all(b"[")?,
        ExportFormat::Markdown => {
            let cells: Vec<String> = names.iter().map(|n| markdown_cell(n)).collect();
            writeln!(out, "| {} |", cells.join(" | "))?;
            let rules: Vec<&str> = metas
                .iter()
                .map(|m| if is_numeric(m.logical) { "---:" } else { "---" })
                .collect();
            writeln!(out, "| {} |", rules.join(" | "))?;
        }
        ExportFormat::Tsv
        | ExportFormat::Csv
        | ExportFormat::SqlInsert { .. }
        | ExportFormat::InList => {}
    }

    let insert_prefix = match &options.format {
        ExportFormat::SqlInsert { table } => format!(
            "INSERT INTO {} ({}) VALUES (",
            dialect.quote_identifier(table),
            metas
                .iter()
                .map(|m| dialect.quote_identifier(&m.name))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => String::new(),
    };

    let mut items: Vec<String> = Vec::new();
    let mut written = 0usize;
    let mut start = rows.start;
    while start < rows.end {
        let len = CHUNK.min(rows.end - start);
        for batch in result.window(start, len) {
            for row in 0..batch.num_rows() {
                let values: Vec<Value> = columns
                    .clone()
                    .map(|c| {
                        Value::from_array(batch.column(c).as_ref(), row, schema.columns[c].logical)
                            .unwrap_or(Value::Null)
                    })
                    .collect();
                match &options.format {
                    ExportFormat::Tsv | ExportFormat::Csv => {
                        let fields: Vec<String> = values
                            .iter()
                            .map(|v| field(&options.format, text(v).as_deref()))
                            .collect();
                        write_line(out, &options.format, &fields)?;
                    }
                    ExportFormat::Json => {
                        out.write_all(if written == 0 { b"\n  {" } else { b",\n  {" })?;
                        for (i, (name, value)) in names.iter().zip(&values).enumerate() {
                            if i > 0 {
                                out.write_all(b", ")?;
                            }
                            write!(
                                out,
                                "{}: {}",
                                json_string(name),
                                json_value(value, metas[i].logical)
                            )?;
                        }
                        out.write_all(b"}")?;
                    }
                    ExportFormat::SqlInsert { .. } => {
                        out.write_all(insert_prefix.as_bytes())?;
                        let literals: Vec<String> = values
                            .iter()
                            .map(|v| sql_literal(v, dialect.name()))
                            .collect();
                        out.write_all(literals.join(", ").as_bytes())?;
                        out.write_all(b");\n")?;
                    }
                    ExportFormat::InList => {
                        let literals: Vec<String> = values
                            .iter()
                            .map(|v| sql_literal(v, dialect.name()))
                            .collect();
                        let item = if literals.len() == 1 {
                            literals.join("")
                        } else {
                            format!("({})", literals.join(", "))
                        };
                        if !items.contains(&item) {
                            items.push(item);
                        }
                    }
                    ExportFormat::Markdown => {
                        let cells: Vec<String> = values
                            .iter()
                            .map(|v| markdown_cell(&text(v).unwrap_or_else(|| "NULL".into())))
                            .collect();
                        writeln!(out, "| {} |", cells.join(" | "))?;
                    }
                }
                written += 1;
            }
        }
        start += len;
    }
    if options.format == ExportFormat::Json {
        out.write_all(if written == 0 { b"]\n" } else { b"\n]\n" })?;
    }
    if options.format == ExportFormat::InList {
        write!(out, "({})", items.join(", "))?;
    }
    Ok(written)
}

/// Writes the export to `path` through a temporary file renamed at the end, so an error or
/// a crash never leaves a half-written file under the final name. CSV starts with a UTF-8
/// byte order mark so spreadsheets (Excel) read accents correctly.
pub fn export_to_file(
    result: &StoredResult,
    options: &ExportOptions,
    dialect: &dyn Dialect,
    path: &std::path::Path,
) -> io::Result<usize> {
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".part");
    let temporary = std::path::PathBuf::from(temporary);
    let written = (|| {
        let mut out = io::BufWriter::new(std::fs::File::create(&temporary)?);
        if options.format == ExportFormat::Csv {
            out.write_all("\u{feff}".as_bytes())?;
        }
        let rows = write_result(result, options, dialect, &mut out)?;
        out.flush()?;
        Ok::<_, io::Error>(rows)
    })();
    match written {
        Ok(rows) => {
            std::fs::rename(&temporary, path)?;
            Ok(rows)
        }
        Err(e) => {
            let _ = std::fs::remove_file(&temporary);
            Err(e)
        }
    }
}

/// A Markdown table cell: `|` escaped, line breaks as `<br>`, backslashes kept literal.
fn markdown_cell(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('|', "\\|")
        .replace("\r\n", "<br>")
        .replace(['\n', '\r'], "<br>")
}

fn is_numeric(logical: LogicalType) -> bool {
    match logical {
        LogicalType::Int | LogicalType::Float | LogicalType::Decimal => true,
        LogicalType::Bool
        | LogicalType::Text
        | LogicalType::Bytes
        | LogicalType::Date
        | LogicalType::Time
        | LogicalType::Timestamp
        | LogicalType::TimestampTz
        | LogicalType::Uuid
        | LogicalType::Json
        | LogicalType::Interval
        | LogicalType::Array
        | LogicalType::Native => false,
    }
}

/// Repeated column names (`id`, `id`) become `id`, `id_2` so JSON keys stay distinct.
fn unique_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let mut candidate = name.to_string();
        let mut n = 2;
        while out.contains(&candidate) {
            candidate = format!("{name}_{n}");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        other => Some(other.to_string()),
    }
}

/// One TSV/CSV field. Quoted only when needed, doubling `"` (spreadsheets read both).
fn field(format: &ExportFormat, value: Option<&str>) -> String {
    let Some(value) = value else {
        return String::new();
    };
    let separator = if *format == ExportFormat::Tsv {
        '\t'
    } else {
        ','
    };
    if value.contains([separator, '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn write_line(out: &mut dyn Write, format: &ExportFormat, fields: &[String]) -> io::Result<()> {
    let (separator, end) = match format {
        ExportFormat::Csv => (",", "\r\n"),
        _ => ("\t", "\n"),
    };
    out.write_all(fields.join(separator).as_bytes())?;
    out.write_all(end.as_bytes())
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Numbers and booleans as JSON values; JSON columns embedded as JSON; exact decimals and
/// everything else as strings (a JSON number could lose a decimal's precision).
fn json_value(value: &Value, logical: LogicalType) -> String {
    match value {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => value.to_string(),
        other => {
            let text = other.to_string();
            let is_json = logical == LogicalType::Json
                && matches!(
                    text.trim_start().chars().next(),
                    Some('{' | '[' | '"' | 't' | 'f' | 'n' | '-' | '0'..='9')
                )
                && json_is_valid(&text);
            if is_json { text } else { json_string(&text) }
        }
    }
}

/// Minimal check that a JSON column's text can be embedded as is: balanced brackets outside
/// strings. The server already validated it as JSON; this only guards against surprises.
fn json_is_valid(text: &str) -> bool {
    let mut depth: i64 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' | '[' => depth += 1,
            '}' | ']' => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0 && !in_string
}

/// A literal for `INSERT`, following the engine (`dialect` is the [`Dialect::name`]).
pub(crate) fn sql_literal(value: &Value, dialect: &str) -> String {
    let quoted = |text: &str| {
        let text = text.replace('\'', "''");
        // MySQL treats `\` as an escape inside strings (default `sql_mode`).
        let text = if dialect == "mysql" {
            text.replace('\\', "\\\\")
        } else {
            text
        };
        format!("'{text}'")
    };
    match value {
        Value::Null => "NULL".into(),
        Value::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => value.to_string(),
        Value::Decimal(d) => d.clone(),
        Value::Bytes(bytes) => {
            let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
            match dialect {
                "postgres" => format!("'\\x{hex}'"),
                "duckdb" => format!("from_hex('{hex}')"),
                _ => format!("X'{hex}'"),
            }
        }
        other => quoted(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow::array::{
        ArrayRef, BinaryArray, BooleanArray, Float64Array, Int64Array, RecordBatch, StringArray,
    };
    use spelunk_driver_api::{ColumnMeta, ResultSchema, StatementSpan};

    use super::*;
    use crate::ResultStore;

    struct Quoted(&'static str);

    impl Dialect for Quoted {
        fn name(&self) -> &'static str {
            self.0
        }
        fn split_statements(&self, _script: &str) -> Vec<StatementSpan> {
            Vec::new()
        }
        fn quote_identifier(&self, ident: &str) -> String {
            match self.0 {
                "mysql" => format!("`{ident}`"),
                _ => format!("\"{ident}\""),
            }
        }
    }

    fn sample() -> Arc<std::sync::RwLock<StoredResult>> {
        let meta = |name: &str, logical| ColumnMeta {
            name: name.into(),
            logical,
            native_type: logical.as_str().into(),
            native_id: None,
        };
        let schema = Arc::new(ResultSchema::new(vec![
            meta("id", LogicalType::Int),
            meta("name", LogicalType::Text),
            meta("price", LogicalType::Float),
            meta("ok", LogicalType::Bool),
            meta("doc", LogicalType::Json),
            meta("raw", LogicalType::Bytes),
            meta("amount", LogicalType::Decimal),
            meta("id", LogicalType::Int),
        ]));
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from(vec![Some(1), Some(2), None])),
            Arc::new(StringArray::from(vec![
                Some("plain"),
                Some("a,b \"q\"\tc\nd"),
                Some("it's \\ x"),
            ])),
            Arc::new(Float64Array::from(vec![Some(1.5), Some(f64::NAN), None])),
            Arc::new(BooleanArray::from(vec![Some(true), Some(false), None])),
            Arc::new(StringArray::from(vec![
                Some("{\"a\": [1, 2]}"),
                Some("not json {"),
                None,
            ])),
            Arc::new(BinaryArray::from(vec![
                Some(&[0xde_u8, 0xad][..]),
                None,
                Some(&[][..]),
            ])),
            Arc::new(StringArray::from(vec![
                Some("12345678901234567890.1234"),
                None,
                Some("0"),
            ])),
            Arc::new(Int64Array::from(vec![Some(10), Some(20), Some(30)])),
        ];
        let store = ResultStore::new();
        let id = store.create(schema.clone());
        store
            .append(
                id,
                RecordBatch::try_new(schema.arrow.clone(), columns).unwrap(),
            )
            .unwrap();
        store.get(id).unwrap()
    }

    fn render(
        format: ExportFormat,
        rows: Range<usize>,
        columns: Range<usize>,
        header: bool,
        dialect: &'static str,
    ) -> String {
        let result = sample();
        let result = result.read().unwrap();
        let mut out = Vec::new();
        write_result(
            &result,
            &ExportOptions {
                format,
                rows,
                columns,
                header,
            },
            &Quoted(dialect),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn tsv_quotes_only_what_spreadsheets_need() {
        assert_eq!(
            render(ExportFormat::Tsv, 0..3, 0..2, true, "postgres"),
            "id\tname\n1\tplain\n2\t\"a,b \"\"q\"\"\tc\nd\"\n\tit's \\ x\n"
        );
    }

    #[test]
    fn markdown_tables_escape_pipes_and_line_breaks() {
        assert_eq!(
            render(ExportFormat::Markdown, 0..3, 0..3, false, "postgres"),
            "| id | name | price |\n\
             | ---: | --- | ---: |\n\
             | 1 | plain | 1.5 |\n\
             | 2 | a,b \"q\"\tc<br>d | NaN |\n\
             | NULL | it's \\\\ x | NULL |\n"
        );
        assert_eq!(
            render(ExportFormat::Markdown, 0..0, 1..2, false, "postgres"),
            "| name |\n| --- |\n"
        );
    }

    #[test]
    fn in_lists_use_literals_and_drop_repeats() {
        assert_eq!(
            render(ExportFormat::InList, 0..3, 1..2, false, "postgres"),
            "('plain', 'a,b \"q\"\tc\nd', 'it''s \\ x')"
        );
        assert_eq!(
            render(ExportFormat::InList, 0..3, 0..1, false, "postgres"),
            "(1, 2, NULL)"
        );
        assert_eq!(
            render(ExportFormat::InList, 0..2, 0..2, false, "mysql"),
            "((1, 'plain'), (2, 'a,b \"q\"\tc\nd'))"
        );
    }

    #[test]
    fn csv_follows_rfc_4180() {
        assert_eq!(
            render(ExportFormat::Csv, 0..2, 0..3, false, "postgres"),
            "1,plain,1.5\r\n2,\"a,b \"\"q\"\"\tc\nd\",NaN\r\n"
        );
    }

    #[test]
    fn json_keeps_types_and_distinct_keys() {
        let json = render(ExportFormat::Json, 0..3, 0..8, false, "postgres");
        assert!(json.starts_with("[\n  {\"id\": 1, \"name\": \"plain\", \"price\": 1.5, \"ok\": true, \"doc\": {\"a\": [1, 2]}"), "{json}");
        assert!(json.contains("\"raw\": \"\\\\xdead\""), "{json}");
        assert!(
            json.contains("\"amount\": \"12345678901234567890.1234\""),
            "{json}"
        );
        assert!(json.contains("\"id_2\": 10"), "{json}");
        // NaN is not a JSON number; invalid JSON text is a string, not embedded.
        assert!(json.contains("\"price\": \"NaN\""), "{json}");
        assert!(json.contains("\"doc\": \"not json {\""), "{json}");
        assert!(
            json.contains("{\"id\": null, \"name\": \"it's \\\\ x\""),
            "{json}"
        );
        assert!(json.ends_with("}\n]\n"), "{json}");
        assert_eq!(
            render(ExportFormat::Json, 3..3, 0..8, false, "postgres"),
            "[]\n"
        );
    }

    #[test]
    fn inserts_use_each_engine_literals() {
        let table = || ExportFormat::SqlInsert {
            table: "items".into(),
        };
        let pg = render(table(), 0..3, 0..7, false, "postgres");
        assert_eq!(
            pg.lines().next().unwrap(),
            "INSERT INTO \"items\" (\"id\", \"name\", \"price\", \"ok\", \"doc\", \"raw\", \"amount\") VALUES (1, 'plain', 1.5, TRUE, '{\"a\": [1, 2]}', '\\xdead', 12345678901234567890.1234);"
        );
        // A value may contain a newline: statements end with ");\n", not lines.
        assert!(
            pg.contains("'NaN', FALSE, 'not json {', NULL, NULL);\n"),
            "{pg}"
        );
        assert!(
            pg.ends_with("VALUES (NULL, 'it''s \\ x', NULL, NULL, NULL, '\\x', 0);\n"),
            "{pg}"
        );
        let mysql = render(table(), 2..3, 0..2, false, "mysql");
        assert_eq!(
            mysql,
            "INSERT INTO `items` (`id`, `name`) VALUES (NULL, 'it''s \\\\ x');\n"
        );
        let sqlite = render(table(), 0..1, 5..6, false, "sqlite");
        assert_eq!(
            sqlite,
            "INSERT INTO \"items\" (\"raw\") VALUES (X'dead');\n"
        );
    }

    #[test]
    fn files_are_written_whole_with_a_bom_for_csv() {
        let dir = std::env::temp_dir().join(format!("spelunk-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let result = sample();
        let result = result.read().unwrap();
        let options = |format| ExportOptions {
            format,
            rows: 0..usize::MAX,
            columns: 0..2,
            header: true,
        };
        let csv = dir.join("out.csv");
        let rows = export_to_file(
            &result,
            &options(ExportFormat::Csv),
            &Quoted("postgres"),
            &csv,
        )
        .unwrap();
        assert_eq!(rows, 3);
        let bytes = std::fs::read(&csv).unwrap();
        assert!(bytes.starts_with("\u{feff}id,name\r\n1,plain".as_bytes()));
        assert!(!dir.join("out.csv.part").exists());
        let json = dir.join("out.json");
        export_to_file(
            &result,
            &options(ExportFormat::Json),
            &Quoted("postgres"),
            &json,
        )
        .unwrap();
        assert!(
            std::fs::read_to_string(&json)
                .unwrap()
                .starts_with("[\n  {")
        );
        // A failure leaves neither the file nor the temporary one.
        let missing = dir.join("no-such-dir").join("out.csv");
        assert!(
            export_to_file(
                &result,
                &options(ExportFormat::Csv),
                &Quoted("postgres"),
                &missing
            )
            .is_err()
        );
        assert!(!missing.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn ranges_are_clipped_to_the_result() {
        assert_eq!(
            render(ExportFormat::Tsv, 2..99, 7..99, false, "postgres"),
            "30\n"
        );
        assert_eq!(render(ExportFormat::Tsv, 5..9, 0..2, false, "postgres"), "");
    }
}
