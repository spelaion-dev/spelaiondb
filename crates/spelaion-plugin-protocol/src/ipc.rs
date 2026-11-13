//! Rows as Arrow IPC: results from the plugin, and the rows of an import to it.

use std::collections::HashMap;
use std::sync::Arc;

use arrow::array::{Array, ArrayRef, LargeStringArray, LargeStringBuilder, RecordBatch};
use arrow::buffer::Buffer;
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::error::ArrowError;
use arrow::ipc::MessageHeader;
use arrow::ipc::reader::{read_dictionary, read_record_batch};
use arrow::ipc::writer::StreamWriter;
use spelaion_driver_api::{ColumnMeta, DriverError, ResultSchema};

/// Writes one result set as the bytes of an Arrow IPC stream, a message at a time.
pub struct BatchEncoder {
    writer: StreamWriter<Vec<u8>>,
}

impl BatchEncoder {
    /// The encoder and the bytes of the schema, which start the stream.
    pub fn new(schema: &Schema) -> Result<(Self, Vec<u8>), ArrowError> {
        let mut writer = StreamWriter::try_new(Vec::new(), schema)?;
        let start = std::mem::take(writer.get_mut());
        Ok((Self { writer }, start))
    }

    /// The bytes of `batch`.
    pub fn encode(&mut self, batch: &RecordBatch) -> Result<Vec<u8>, ArrowError> {
        self.writer.write(batch)?;
        Ok(std::mem::take(self.writer.get_mut()))
    }
}

/// What the bytes of a stream decode into.
#[derive(Debug)]
pub enum Decoded {
    Schema(SchemaRef),
    Batch(RecordBatch),
}

/// The most one IPC message (its metadata, or its body) may declare. The bytes come from the
/// other program: nothing is kept waiting for a message larger than this.
const MAX_MESSAGE: usize = 1 << 30;

/// The first four bytes of a message: what follows is its length.
const CONTINUATION: [u8; 4] = [0xff; 4];

/// Reads the bytes of one Arrow IPC stream as they arrive, cut anywhere.
///
/// Arrow's own `StreamDecoder` only acts on a message when more bytes follow it, so a schema
/// (a message with no body) would not be known until the first batch arrived: a result with
/// no rows would have no columns. This reads one encapsulated message at a time instead, with
/// Arrow's readers for what is inside.
#[derive(Default)]
pub struct BatchDecoder {
    pending: Vec<u8>,
    schema: Option<SchemaRef>,
    dictionaries: HashMap<i64, ArrayRef>,
}

impl BatchDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// What `bytes` complete, in order. The bytes come from the other program: a stream that
    /// does not decode is an error, never a panic.
    pub fn push(&mut self, bytes: Vec<u8>) -> Result<Vec<Decoded>, ArrowError> {
        if self.pending.is_empty() {
            self.pending = bytes;
        } else {
            self.pending.extend_from_slice(&bytes);
        }
        let mut out = Vec::new();
        let mut at = 0;
        loop {
            let rest = &self.pending[at..];
            if rest.len() < 4 {
                break;
            }
            // `0xFFFFFFFF` and the length; streams written before Arrow 0.15 have only the length.
            let (header, length) = if rest[..4] == CONTINUATION {
                if rest.len() < 8 {
                    break;
                }
                (8, i32::from_le_bytes([rest[4], rest[5], rest[6], rest[7]]))
            } else {
                (4, i32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]))
            };
            if length == 0 {
                // The end of the stream: nothing to read, and what ends a result is a message.
                at += header;
                continue;
            }
            let length = usize::try_from(length)
                .ok()
                .filter(|length| *length <= MAX_MESSAGE)
                .ok_or_else(|| ipc_error("a message of an impossible length"))?;
            if rest.len() < header + length {
                break;
            }
            let message = arrow::ipc::root_as_message(&rest[header..header + length])
                .map_err(|e| ipc_error(&format!("a message that cannot be read: {e}")))?;
            let body = usize::try_from(message.bodyLength())
                .ok()
                .filter(|body| *body <= MAX_MESSAGE)
                .ok_or_else(|| ipc_error("a message with a body of an impossible length"))?;
            let total = header + length + body;
            if rest.len() < total {
                break;
            }
            // A copy into memory aligned as Arrow needs it.
            let body = Buffer::from(&rest[header + length..total]);
            let version = message.version();
            match message.header_type() {
                MessageHeader::Schema => {
                    if self.schema.is_some() {
                        return Err(ipc_error("a second schema in one stream"));
                    }
                    let schema = message
                        .header_as_schema()
                        .ok_or_else(|| ipc_error("a schema that cannot be read"))?;
                    let schema = Arc::new(arrow::ipc::convert::try_fb_to_schema(schema)?);
                    self.schema = Some(schema.clone());
                    out.push(Decoded::Schema(schema));
                }
                MessageHeader::RecordBatch => {
                    let schema = self
                        .schema
                        .clone()
                        .ok_or_else(|| ipc_error("rows before their schema"))?;
                    let batch = message
                        .header_as_record_batch()
                        .ok_or_else(|| ipc_error("rows that cannot be read"))?;
                    out.push(Decoded::Batch(read_record_batch(
                        &body,
                        batch,
                        schema,
                        &self.dictionaries,
                        None,
                        &version,
                    )?));
                }
                MessageHeader::DictionaryBatch => {
                    let schema = self
                        .schema
                        .clone()
                        .ok_or_else(|| ipc_error("a dictionary before its schema"))?;
                    let batch = message
                        .header_as_dictionary_batch()
                        .ok_or_else(|| ipc_error("a dictionary that cannot be read"))?;
                    read_dictionary(&body, batch, &schema, &mut self.dictionaries, &version)?;
                }
                MessageHeader::NONE => {}
                other => {
                    return Err(ipc_error(&format!(
                        "a message of an unexpected type: {other:?}"
                    )));
                }
            }
            at += total;
        }
        self.pending.drain(..at);
        Ok(out)
    }
}

fn ipc_error(message: &str) -> ArrowError {
    ArrowError::IpcError(message.to_string())
}

/// The result's schema from the Arrow schema it travelled as: every field carries the
/// metadata [`ColumnMeta::to_field`] wrote.
pub fn result_schema(arrow: SchemaRef) -> Result<ResultSchema, DriverError> {
    let columns = arrow
        .fields()
        .iter()
        .map(|field| {
            ColumnMeta::from_field(field).ok_or_else(|| {
                DriverError::Other(format!(
                    "the plugin sent the column {:?} without its type",
                    field.name()
                ))
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ResultSchema { columns, arrow })
}

/// The rows of an import as one batch: a nullable text column per target column.
pub fn rows_to_batch(
    columns: usize,
    rows: &[Vec<Option<String>>],
) -> Result<RecordBatch, ArrowError> {
    let schema = Arc::new(Schema::new(
        (0..columns)
            .map(|i| Field::new(format!("c{i}"), DataType::LargeUtf8, true))
            .collect::<Vec<_>>(),
    ));
    let mut builders: Vec<LargeStringBuilder> =
        (0..columns).map(|_| LargeStringBuilder::new()).collect();
    for row in rows {
        if row.len() != columns {
            return Err(ArrowError::InvalidArgumentError(format!(
                "a row has {} values for {columns} columns",
                row.len()
            )));
        }
        for (builder, value) in builders.iter_mut().zip(row) {
            builder.append_option(value.as_deref());
        }
    }
    let arrays: Vec<ArrayRef> = builders
        .into_iter()
        .map(|mut b| Arc::new(b.finish()) as ArrayRef)
        .collect();
    if columns == 0 {
        return RecordBatch::try_new_with_options(
            schema,
            arrays,
            &arrow::array::RecordBatchOptions::new().with_row_count(Some(rows.len())),
        );
    }
    RecordBatch::try_new(schema, arrays)
}

/// The rows of a batch made by [`rows_to_batch`]. Anything else is refused.
pub fn batch_to_rows(batch: &RecordBatch) -> Result<Vec<Vec<Option<String>>>, DriverError> {
    let columns: Vec<&LargeStringArray> = batch
        .columns()
        .iter()
        .map(|column| {
            column
                .as_any()
                .downcast_ref::<LargeStringArray>()
                .ok_or_else(|| DriverError::Other("the rows to load are not text".into()))
        })
        .collect::<Result<_, _>>()?;
    let mut rows = Vec::with_capacity(batch.num_rows());
    for row in 0..batch.num_rows() {
        rows.push(
            columns
                .iter()
                .map(|column| (!column.is_null(row)).then(|| column.value(row).to_string()))
                .collect(),
        );
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use arrow::array::Int64Array;
    use spelaion_driver_api::{ColumnOrigin, LogicalType};

    use super::*;

    fn schema() -> ResultSchema {
        ResultSchema::new(vec![
            ColumnMeta {
                name: "id".into(),
                logical: LogicalType::Int,
                native_type: "int8".into(),
                native_id: Some(20),
                origin: Some(ColumnOrigin {
                    table: "t".into(),
                    column: "id".into(),
                }),
            },
            ColumnMeta {
                name: "geom".into(),
                logical: LogicalType::Native,
                native_type: "geometry".into(),
                native_id: None,
                origin: None,
            },
        ])
    }

    fn batch(schema: &ResultSchema, ids: &[i64]) -> RecordBatch {
        let texts: Vec<Option<String>> =
            ids.iter().map(|i| Some(format!("POINT({i} 0)"))).collect();
        RecordBatch::try_new(
            schema.arrow.clone(),
            vec![
                Arc::new(Int64Array::from(ids.to_vec())),
                Arc::new(arrow::array::StringArray::from(texts)),
            ],
        )
        .unwrap()
    }

    #[test]
    fn a_result_keeps_its_types_and_rows_through_the_wire() {
        let schema = schema();
        let (mut encoder, start) = BatchEncoder::new(&schema.arrow).unwrap();
        let first = encoder.encode(&batch(&schema, &[1, 2])).unwrap();
        let second = encoder.encode(&batch(&schema, &[3])).unwrap();

        let mut decoder = BatchDecoder::new();
        let decoded = decoder.push(start).unwrap();
        let [Decoded::Schema(arrow)] = decoded.as_slice() else {
            panic!("{decoded:?}");
        };
        // Native type, type id and origin are all still there.
        assert_eq!(result_schema(arrow.clone()).unwrap(), schema);
        let decoded = decoder.push(first).unwrap();
        let [Decoded::Batch(rows)] = decoded.as_slice() else {
            panic!("{decoded:?}");
        };
        assert_eq!(rows, &batch(&schema, &[1, 2]));
        let decoded = decoder.push(second).unwrap();
        let [Decoded::Batch(rows)] = decoded.as_slice() else {
            panic!("{decoded:?}");
        };
        assert_eq!(rows.num_rows(), 1);
    }

    #[test]
    fn bytes_cut_anywhere_decode_the_same() {
        let schema = schema();
        let (mut encoder, mut wire) = BatchEncoder::new(&schema.arrow).unwrap();
        wire.extend(encoder.encode(&batch(&schema, &[1, 2, 3])).unwrap());
        for chunk in [1, 7, 64, wire.len()] {
            let mut decoder = BatchDecoder::new();
            let mut rows = 0;
            let mut schemas = 0;
            for piece in wire.chunks(chunk) {
                for item in decoder.push(piece.to_vec()).unwrap() {
                    match item {
                        Decoded::Schema(_) => schemas += 1,
                        Decoded::Batch(batch) => rows += batch.num_rows(),
                    }
                }
            }
            assert_eq!((schemas, rows), (1, 3), "chunks of {chunk}");
        }
    }

    #[test]
    fn bytes_that_are_not_arrow_are_an_error() {
        // A message of 16 bytes that is not one.
        let mut wire = vec![0xff, 0xff, 0xff, 0xff, 16, 0, 0, 0];
        wire.extend([0x5a; 16]);
        assert!(BatchDecoder::new().push(wire).is_err());
        // A length that cannot be.
        assert!(BatchDecoder::new().push(vec![0xff; 8]).is_err());
        // Rows before any schema.
        let schema = schema();
        let (mut encoder, _) = BatchEncoder::new(&schema.arrow).unwrap();
        let rows = encoder.encode(&batch(&schema, &[1])).unwrap();
        assert!(BatchDecoder::new().push(rows).is_err());
    }

    #[test]
    fn a_column_without_its_type_is_refused() {
        let arrow = Arc::new(Schema::new(vec![Field::new("a", DataType::Utf8, true)]));
        assert!(result_schema(arrow).is_err());
    }

    #[test]
    fn the_rows_of_an_import_round_trip_with_their_nulls() {
        let rows = vec![
            vec![Some("1".to_string()), None],
            vec![Some(String::new()), Some("ñ\n'\"".to_string())],
        ];
        let batch = rows_to_batch(2, &rows).unwrap();
        assert_eq!(batch_to_rows(&batch).unwrap(), rows);
        // No columns at all still counts its rows.
        let empty = rows_to_batch(0, &[vec![], vec![]]).unwrap();
        assert_eq!(batch_to_rows(&empty).unwrap(), vec![Vec::new(), Vec::new()]);
        // A row of another width is refused.
        assert!(rows_to_batch(2, &[vec![None]]).is_err());
    }
}
