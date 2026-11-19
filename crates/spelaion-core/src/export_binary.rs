//! Parquet and Arrow IPC files (SDB-391): the rows are Arrow already (ADR-005), so a batch is
//! written as it is, its columns typed from their logical and native type. A batch is written
//! and dropped before the next one: Parquet closes a row group after every batch, so the
//! export holds one batch at a time as every streaming export does (ADR-034). The file goes
//! through the same [`PartFile`] as the text formats: written as `<name>.part` and renamed
//! only when complete.

use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use arrow::array::{Array, ArrayRef, Decimal128Builder, RecordBatch, StringArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::ipc::writer::FileWriter;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, GzipLevel};
use parquet::file::properties::WriterProperties;
use spelaion_driver_api::{ColumnMeta, LogicalType};

use crate::export::{Encoder, ExportFormat, FileSummary, PartFile, Sink, TextEncoding};

/// Largest precision Arrow's `Decimal128` holds.
const MAX_DECIMAL128: u8 = 38;

/// How one column is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    /// As the core holds it: the logical type's Arrow type.
    Same,
    /// Decimal text written as `Decimal128(precision, scale)`, from a native type that
    /// declares both (`numeric(10,2)`). Without them a decimal stays text: exact, whatever its
    /// size.
    Decimal { precision: u8, scale: i8 },
}

/// The precision and scale a native type declares: `numeric(10,2)` → `(10, 2)`,
/// `decimal(12)` → `(12, 0)`. `None` without them, or past what `Decimal128` holds.
fn declared_decimal(native_type: &str) -> Option<(u8, i8)> {
    let open = native_type.find('(')?;
    let close = native_type[open..].find(')')? + open;
    let inside = &native_type[open + 1..close];
    let mut parts = inside.split(',').map(str::trim);
    let precision: u8 = parts.next()?.parse().ok()?;
    let scale: i8 = match parts.next() {
        Some(scale) => scale.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some()
        || precision == 0
        || precision > MAX_DECIMAL128
        || scale < 0
        || scale as u8 > precision
    {
        return None;
    }
    Some((precision, scale))
}

/// A decimal's text as the integer `Decimal128` stores at `scale`: `-12.5` at scale 2 is
/// `-1250`. `None` for what the declared type cannot hold exactly.
fn decimal_units(text: &str, precision: u8, scale: i8) -> Option<i128> {
    let text = text.trim();
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    if whole.is_empty() && fraction.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let scale = scale as usize;
    // Trailing zeros past the scale say nothing; anything else would be rounded.
    let fraction = fraction.trim_end_matches('0');
    if fraction.len() > scale {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    if whole.len() + scale > precision as usize {
        return None;
    }
    let mut units: i128 = 0;
    for b in whole.bytes().chain(fraction.bytes()) {
        units = units.checked_mul(10)?.checked_add(i128::from(b - b'0'))?;
    }
    for _ in fraction.len()..scale {
        units = units.checked_mul(10)?;
    }
    Some(if negative { -units } else { units })
}

/// Column names a file can hold: repeated ones get `_2`, `_3`… (Parquet refuses two columns
/// of the same name).
fn unique(names: impl Iterator<Item = String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for name in names {
        let mut candidate = name.clone();
        let mut n = 2;
        while out.contains(&candidate) {
            candidate = format!("{name}_{n}");
            n += 1;
        }
        out.push(candidate);
    }
    out
}

enum Writer {
    Parquet(ArrowWriter<Encoder<Sink>>),
    Arrow(FileWriter<Encoder<Sink>>),
}

/// A Parquet or Arrow IPC file being written, a batch at a time.
pub(crate) struct BinaryFile {
    writer: Writer,
    /// The result's columns written, by position, in this order.
    columns: Vec<usize>,
    kinds: Vec<Column>,
    names: Vec<String>,
    schema: SchemaRef,
    rows: u64,
}

impl BinaryFile {
    /// `fields` are the result's fields (with the core's metadata) of `columns`, in order.
    /// `gzip`: Parquet's column chunks are compressed with its own GZIP codec, so every Parquet
    /// reader opens the file; an Arrow file is one gzip stream (`.arrow.gz`).
    pub(crate) fn create(
        path: &Path,
        format: &ExportFormat,
        gzip: bool,
        fields: &[Field],
        columns: Vec<usize>,
        batch_rows: usize,
        progress: Option<Arc<AtomicU64>>,
    ) -> io::Result<Self> {
        let names = unique(fields.iter().map(|f| f.name().clone()));
        let mut kinds = Vec::with_capacity(fields.len());
        let mut out_fields = Vec::with_capacity(fields.len());
        for (field, name) in fields.iter().zip(&names) {
            let meta = ColumnMeta::from_field(field);
            let kind = match meta {
                Some(meta) if meta.logical == LogicalType::Decimal => {
                    match declared_decimal(&meta.native_type) {
                        Some((precision, scale)) => Column::Decimal { precision, scale },
                        None => Column::Same,
                    }
                }
                Some(_) | None => Column::Same,
            };
            let data_type = match kind {
                Column::Same => field.data_type().clone(),
                Column::Decimal { precision, scale } => DataType::Decimal128(precision, scale),
            };
            out_fields
                .push(Field::new(name, data_type, true).with_metadata(field.metadata().clone()));
            kinds.push(kind);
        }
        let schema: SchemaRef = Arc::new(Schema::new(out_fields));
        let mut file = PartFile::create(
            path,
            TextEncoding::Utf8,
            gzip && *format == ExportFormat::ArrowIpc,
        )?;
        if let Some(counter) = progress {
            file.report_to(counter);
        }
        let out = file.into_writer();
        let writer = match format {
            ExportFormat::Parquet => {
                let compression = if gzip {
                    Compression::GZIP(GzipLevel::default())
                } else {
                    Compression::UNCOMPRESSED
                };
                let properties = WriterProperties::builder()
                    .set_max_row_group_row_count(Some(batch_rows.max(1)))
                    .set_compression(compression)
                    .build();
                Writer::Parquet(
                    ArrowWriter::try_new(out, schema.clone(), Some(properties))
                        .map_err(io::Error::other)?,
                )
            }
            ExportFormat::ArrowIpc => {
                Writer::Arrow(FileWriter::try_new(out, &schema).map_err(io::Error::other)?)
            }
            ExportFormat::Tsv
            | ExportFormat::Csv
            | ExportFormat::Json
            | ExportFormat::SqlInsert { .. }
            | ExportFormat::SqlInsertBatch { .. }
            | ExportFormat::Markdown
            | ExportFormat::InList
            | ExportFormat::Html
            | ExportFormat::Ndjson
            | ExportFormat::Xlsx => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "only Parquet and Arrow are written a batch at a time",
                ));
            }
        };
        Ok(Self {
            writer,
            columns,
            kinds,
            names,
            schema,
            rows: 0,
        })
    }

    /// Writes the chosen columns of `batch`; for Parquet, as a row group of its own.
    pub(crate) fn write(&mut self, batch: &RecordBatch) -> io::Result<()> {
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(self.columns.len());
        for ((&column, kind), name) in self.columns.iter().zip(&self.kinds).zip(&self.names) {
            let array = batch.column(column).clone();
            arrays.push(match *kind {
                Column::Same => array,
                Column::Decimal { precision, scale } => {
                    decimals(array.as_ref(), precision, scale, name)?
                }
            });
        }
        let out = RecordBatch::try_new(self.schema.clone(), arrays).map_err(io::Error::other)?;
        match &mut self.writer {
            Writer::Parquet(writer) => {
                writer.write(&out).map_err(io::Error::other)?;
                // The row group is written now: nothing of this batch stays in memory.
                writer.flush().map_err(io::Error::other)?;
            }
            Writer::Arrow(writer) => writer.write(&out).map_err(io::Error::other)?,
        }
        self.rows += out.num_rows() as u64;
        Ok(())
    }

    /// Ends the file (Parquet's footer, Arrow's) and renames it to its final name.
    pub(crate) fn finish(self) -> io::Result<FileSummary> {
        let out = match self.writer {
            Writer::Parquet(writer) => writer.into_inner().map_err(io::Error::other)?,
            Writer::Arrow(mut writer) => {
                writer.finish().map_err(io::Error::other)?;
                writer.into_inner().map_err(io::Error::other)?
            }
        };
        PartFile::from_writer(out).commit(self.rows)
    }
}

/// Decimal text as `Decimal128`; a value the declared type cannot hold exactly (PostgreSQL's
/// `NaN`, a value SQLite stored as it came) stops the export, naming the column.
fn decimals(array: &dyn Array, precision: u8, scale: i8, column: &str) -> io::Result<ArrayRef> {
    let text = array
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| io::Error::other(format!("{column}: a decimal column holds no text")))?;
    let mut builder = Decimal128Builder::with_capacity(text.len());
    for row in 0..text.len() {
        if text.is_null(row) {
            builder.append_null();
            continue;
        }
        let value = text.value(row);
        let units = decimal_units(value, precision, scale).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{column}: {value} is not a decimal({precision},{scale}), the column's type: export it as CSV or JSON"
                ),
            )
        })?;
        builder.append_value(units);
    }
    let array = builder
        .finish()
        .with_precision_and_scale(precision, scale)
        .map_err(io::Error::other)?;
    Ok(Arc::new(array))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declared_decimals_are_read_from_the_native_type() {
        assert_eq!(declared_decimal("numeric(10,2)"), Some((10, 2)));
        assert_eq!(declared_decimal("DECIMAL(12, 4)"), Some((12, 4)));
        assert_eq!(declared_decimal("decimal(12)"), Some((12, 0)));
        assert_eq!(declared_decimal("numeric"), None);
        assert_eq!(declared_decimal("numeric(39,2)"), None);
        assert_eq!(declared_decimal("numeric(2,3)"), None);
        assert_eq!(declared_decimal("numeric(0)"), None);
        assert_eq!(declared_decimal("numeric(10,2,1)"), None);
        assert_eq!(declared_decimal("numeric(a,b)"), None);
    }

    #[test]
    fn decimal_text_becomes_exact_units() {
        assert_eq!(decimal_units("12.50", 10, 2), Some(1250));
        assert_eq!(decimal_units("-0.25", 10, 2), Some(-25));
        assert_eq!(decimal_units("0", 10, 2), Some(0));
        assert_eq!(decimal_units("+7", 3, 0), Some(7));
        assert_eq!(decimal_units(".5", 3, 1), Some(5));
        assert_eq!(decimal_units("1.230000", 5, 2), Some(123));
        assert_eq!(
            decimal_units("99999999999999999999999999999999999999", 38, 0),
            Some(99_999_999_999_999_999_999_999_999_999_999_999_999)
        );
        // Never rounded, never past the precision, never anything but digits.
        assert_eq!(decimal_units("1.235", 10, 2), None);
        assert_eq!(decimal_units("1234", 5, 2), None);
        assert_eq!(decimal_units("NaN", 10, 2), None);
        assert_eq!(decimal_units("1e5", 10, 2), None);
        assert_eq!(decimal_units("", 10, 2), None);
        assert_eq!(decimal_units("-", 10, 2), None);
        assert_eq!(decimal_units("1.2.3", 10, 2), None);
    }
}
