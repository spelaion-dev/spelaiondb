use std::collections::HashMap;
use std::sync::Arc;

use arrow::datatypes::{Field, Schema, SchemaRef};

use crate::LogicalType;

const META_LOGICAL: &str = "spelaion.logical";
const META_NATIVE_TYPE: &str = "spelaion.native_type";
const META_NATIVE_ID: &str = "spelaion.native_id";

/// Metadata of a result column: common logical type + the engine's native type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnMeta {
    pub name: String,
    pub logical: LogicalType,
    /// Type name in the engine (`int4`, `geometry`, `mood`…).
    pub native_type: String,
    /// Numeric type identifier in the engine (OID in PostgreSQL).
    pub native_id: Option<u32>,
}

impl ColumnMeta {
    /// Arrow field with the metadata embedded, so it travels with the data.
    pub fn to_field(&self) -> Field {
        let mut meta = HashMap::from([
            (META_LOGICAL.to_string(), self.logical.as_str().to_string()),
            (META_NATIVE_TYPE.to_string(), self.native_type.clone()),
        ]);
        if let Some(id) = self.native_id {
            meta.insert(META_NATIVE_ID.to_string(), id.to_string());
        }
        Field::new(&self.name, self.logical.arrow_type(), true).with_metadata(meta)
    }

    /// Recovers the metadata of a field created with [`ColumnMeta::to_field`].
    pub fn from_field(field: &Field) -> Option<Self> {
        let meta = field.metadata();
        Some(Self {
            name: field.name().clone(),
            logical: LogicalType::parse(meta.get(META_LOGICAL)?)?,
            native_type: meta.get(META_NATIVE_TYPE)?.clone(),
            native_id: meta.get(META_NATIVE_ID).and_then(|v| v.parse().ok()),
        })
    }
}

/// Schema of a result set: readable metadata + the equivalent Arrow schema.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSchema {
    pub columns: Vec<ColumnMeta>,
    pub arrow: SchemaRef,
}

impl ResultSchema {
    pub fn new(columns: Vec<ColumnMeta>) -> Self {
        let arrow = Arc::new(Schema::new(
            columns.iter().map(ColumnMeta::to_field).collect::<Vec<_>>(),
        ));
        Self { columns, arrow }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_round_trips_through_arrow() {
        let col = ColumnMeta {
            name: "geom".into(),
            logical: LogicalType::Native,
            native_type: "geometry".into(),
            native_id: Some(18_000),
        };
        let schema = ResultSchema::new(vec![col.clone()]);
        assert_eq!(ColumnMeta::from_field(schema.arrow.field(0)), Some(col));
    }
}
