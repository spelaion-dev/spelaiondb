//! Column types across engines (ADR-069): a source column becomes a portable kind, from its
//! logical type (what the plugin reads it as) and its native type (sizes, precision, length,
//! unsigned); the kind becomes a type of the target engine, with every loss it has there named.
//! Engines are named by their dialect (`postgres`, `mysql` for MySQL and MariaDB, `sqlserver`,
//! `sqlite`, `duckdb`, `clickhouse`).

use spelaion_driver_api::LogicalType;

/// What a column holds, whatever engine it is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Portable {
    Bool,
    /// An integer of `bytes` (1, 2, 4 or 8) bytes.
    Int {
        bytes: u8,
        unsigned: bool,
    },
    /// Exact. `None` when the type gives none (PostgreSQL's bare `numeric`).
    Decimal {
        precision: Option<u32>,
        scale: Option<u32>,
    },
    /// 4 or 8 bytes.
    Float {
        bytes: u8,
    },
    /// `length` in characters, when the type has one.
    Text {
        length: Option<u32>,
    },
    Binary {
        length: Option<u32>,
    },
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    /// Intervals, arrays, an engine's own types: copied as their text.
    Other,
}

/// What a column loses on its way to the target, said on that column in the preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Loss {
    /// The target has no type that keeps a zone: the value is stored as its UTC time.
    ZoneToUtc,
    /// No UUID type: stored as its text.
    UuidAsText,
    /// No JSON type: stored as its text.
    JsonAsText,
    /// No time-of-day type: stored as its text.
    TimeAsText,
    /// The source's own type (an interval, an array…) has no counterpart: stored as its text.
    AsText { native: String },
    /// Wider than the target allows: its widest, and a value that does not fit fails the copy.
    DecimalNarrowed { precision: u32, scale: u32 },
    /// A decimal with no precision of its own, on a target that needs one: its widest.
    DecimalBounded { precision: u32, scale: u32 },
    /// The target keeps the number in its own numeric storage, which may round decimals beyond
    /// 15 significant digits (SQLite).
    DecimalApproximate,
    /// The target's text has no length: any length is kept.
    LengthDropped,
    /// Binary columns cannot be copied to this target yet (ADR-069): leave it out.
    BinaryNotYet,
}

impl Loss {
    /// Whether the column cannot be copied at all as it is (the user must leave it out).
    pub fn blocks(&self) -> bool {
        match self {
            Loss::BinaryNotYet => true,
            Loss::ZoneToUtc
            | Loss::UuidAsText
            | Loss::JsonAsText
            | Loss::TimeAsText
            | Loss::AsText { .. }
            | Loss::DecimalNarrowed { .. }
            | Loss::DecimalBounded { .. }
            | Loss::DecimalApproximate
            | Loss::LengthDropped => false,
        }
    }
}

/// A column's type on the target, and what it loses there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TargetType {
    pub sql_type: String,
    pub losses: Vec<Loss>,
}

/// The base name and the numbers in parentheses of a native type, lower case:
/// `numeric(12, 2)` → (`numeric`, [12, 2]); `int unsigned` → (`int unsigned`, []).
fn parts(native: &str) -> (String, Vec<u32>) {
    let lower = native.trim().to_ascii_lowercase();
    let Some(open) = lower.find('(') else {
        return (lower, Vec::new());
    };
    let close = lower[open..].find(')').map_or(lower.len(), |i| open + i);
    let numbers = lower[open + 1..close]
        .split(',')
        .filter_map(|n| n.trim().parse().ok())
        .collect();
    let rest = lower.get(close + 1..).unwrap_or("").trim();
    let base = format!("{} {rest}", lower[..open].trim());
    (base.trim().to_string(), numbers)
}

/// What a source column holds, from the engine it is on (its dialect's name), its native type
/// and the logical type its plugin reads it as.
pub fn portable(engine: &str, native: &str, logical: LogicalType) -> Portable {
    // ClickHouse's wrappers say nothing of the values.
    let native = unwrap_clickhouse(native);
    let (base, numbers) = parts(native);
    let first = numbers.first().copied();
    // SQLite types a result by its values; a read of no rows has only the declared type, which
    // it reports as text. The declared type then says what the column holds.
    let logical = match logical {
        LogicalType::Text if engine == "sqlite" => sqlite_declared(&base),
        other => other,
    };
    match logical {
        LogicalType::Bool => Portable::Bool,
        LogicalType::Int => int_kind(engine, &base),
        LogicalType::Float => Portable::Float {
            bytes: match base.as_str() {
                // SQLite's `REAL` is its 8-byte float.
                "real" if engine == "sqlite" => 8,
                "real" | "float4" | "float32" => 4,
                // MySQL's `float` is single precision; SQL Server's and PostgreSQL's double.
                "float" if engine == "mysql" => 4,
                "float" if first.is_some_and(|p| p <= 24) => 4,
                _ => 8,
            },
        },
        LogicalType::Decimal => match base.as_str() {
            // SQL Server's money types are decimals with four places.
            "money" => Portable::Decimal {
                precision: Some(19),
                scale: Some(4),
            },
            "smallmoney" => Portable::Decimal {
                precision: Some(10),
                scale: Some(4),
            },
            // ClickHouse's `Decimal32(S)`… give the scale; the precision is the size's.
            "decimal32" => Portable::Decimal {
                precision: Some(9),
                scale: first,
            },
            "decimal64" => Portable::Decimal {
                precision: Some(18),
                scale: first,
            },
            "decimal128" => Portable::Decimal {
                precision: Some(38),
                scale: first,
            },
            "decimal256" => Portable::Decimal {
                precision: Some(76),
                scale: first,
            },
            _ => Portable::Decimal {
                precision: first,
                scale: numbers.get(1).copied().or(first.map(|_| 0)),
            },
        },
        LogicalType::Text => match base.as_str() {
            "time" | "time without time zone" => Portable::Time,
            _ => Portable::Text {
                length: text_length(&base, first),
            },
        },
        LogicalType::Bytes => Portable::Binary { length: first },
        LogicalType::Date => Portable::Date,
        LogicalType::Time => Portable::Time,
        LogicalType::Timestamp => Portable::Timestamp,
        LogicalType::TimestampTz => Portable::TimestampTz,
        LogicalType::Uuid => Portable::Uuid,
        LogicalType::Json => Portable::Json,
        LogicalType::Interval | LogicalType::Array | LogicalType::Native => Portable::Other,
    }
}

/// What a SQLite column holds by its declared type (lower case, sizes taken off), as SQLite's
/// own affinity rules read the name, refined by the names that say more (`boolean`, `date`).
fn sqlite_declared(base: &str) -> LogicalType {
    match base {
        "boolean" | "bool" => LogicalType::Bool,
        "date" => LogicalType::Date,
        "time" => LogicalType::Time,
        "datetime" | "timestamp" => LogicalType::Timestamp,
        "timestamptz" | "timestamp with time zone" => LogicalType::TimestampTz,
        "decimal" | "numeric" => LogicalType::Decimal,
        "uuid" => LogicalType::Uuid,
        "json" => LogicalType::Json,
        name if name.contains("int") => LogicalType::Int,
        // No declared type holds anything: its values travel as text.
        name if name.contains("char")
            || name.contains("clob")
            || name.contains("text")
            || name.is_empty() =>
        {
            LogicalType::Text
        }
        name if name.contains("blob") => LogicalType::Bytes,
        name if name.contains("real") || name.contains("floa") || name.contains("doub") => {
            LogicalType::Float
        }
        _ => LogicalType::Decimal,
    }
}

/// An integer column's size and sign from its base type name (lower case). Each engine names
/// its sizes its own way: PostgreSQL's `int8` is 8 bytes, ClickHouse's `Int8` one.
fn int_kind(engine: &str, base: &str) -> Portable {
    let int = |bytes: u8, unsigned: bool| Portable::Int { bytes, unsigned };
    match engine {
        // `Int8`…`Int64`, `UInt8`…`UInt64`; wider ones are 8 bytes here and checked by the
        // server when they land.
        "clickhouse" => {
            let unsigned = base.starts_with("uint");
            let bytes = match base.trim_start_matches('u') {
                "int8" => 1,
                "int16" => 2,
                "int32" => 4,
                _ => 8,
            };
            int(bytes, unsigned)
        }
        // SQLite's integers are 64-bit, whatever their declared name.
        "sqlite" => int(8, false),
        // SQL Server's `tinyint` is 0–255.
        "sqlserver" => match base {
            "tinyint" => int(1, true),
            "smallint" => int(2, false),
            "int" => int(4, false),
            _ => int(8, false),
        },
        "duckdb" => match base {
            "tinyint" | "int1" => int(1, false),
            "smallint" | "int2" | "short" => int(2, false),
            "integer" | "int" | "int4" | "signed" => int(4, false),
            "utinyint" => int(1, true),
            "usmallint" => int(2, true),
            "uinteger" => int(4, true),
            "ubigint" => int(8, true),
            _ => int(8, false),
        },
        // MySQL and MariaDB write `unsigned` after the name; PostgreSQL has no unsigned types.
        _ => {
            let unsigned = base.ends_with(" unsigned");
            let name = base.trim_end_matches(" unsigned").trim();
            let bytes = match name {
                "tinyint" => 1,
                "smallint" | "int2" | "smallserial" | "year" => 2,
                "mediumint" | "int" | "integer" | "int4" | "serial" => 4,
                _ => 8,
            };
            int(bytes, unsigned)
        }
    }
}

/// `Nullable(T)` and `LowCardinality(T)` are `T`.
/// Whether a column of `native` type on `engine` (a dialect's name) stores bytes as they are: where
/// a copy sends a binary column's bytes (SDB-464). Into any other column they go as their text.
/// ClickHouse's `String` holds any bytes; SQLite's column with no declared type keeps a blob.
pub fn takes_bytes(engine: &str, native: &str) -> bool {
    let (base, _) = parts(unwrap_clickhouse(native));
    match engine {
        "postgres" => base == "bytea",
        "mysql" => matches!(
            base.as_str(),
            "binary" | "varbinary" | "tinyblob" | "blob" | "mediumblob" | "longblob"
        ),
        "sqlserver" => matches!(base.as_str(), "binary" | "varbinary" | "image"),
        "sqlite" => base.is_empty() || base.contains("blob"),
        "duckdb" => matches!(base.as_str(), "blob" | "bytea" | "binary" | "varbinary"),
        "clickhouse" => base == "string" || base == "fixedstring",
        _ => false,
    }
}

fn unwrap_clickhouse(native: &str) -> &str {
    let mut native = native.trim();
    for wrapper in ["Nullable(", "LowCardinality("] {
        while let Some(inner) = native
            .strip_prefix(wrapper)
            .and_then(|rest| rest.strip_suffix(')'))
        {
            native = inner.trim();
        }
    }
    native
}

/// A text type's length in characters, when it has a meaningful one (`varchar(80)`, `char(2)`,
/// `nvarchar(40)`, ClickHouse's `FixedString(8)`), never `max` or a text without one.
fn text_length(base: &str, first: Option<u32>) -> Option<u32> {
    let bounded = matches!(
        base,
        "varchar"
            | "character varying"
            | "char"
            | "character"
            | "nvarchar"
            | "nchar"
            | "national character varying"
            | "fixedstring"
            | "bpchar"
    );
    first.filter(|_| bounded)
}

/// The type a column of `kind` gets on `target` (a dialect's name), `nullable` or not, and
/// what it loses there.
pub fn target_type(target: &str, kind: Portable, nullable: bool) -> TargetType {
    let mut losses = Vec::new();
    let sql_type = match target {
        "postgres" => postgres(kind, &mut losses),
        "mysql" => mysql(kind, &mut losses),
        "sqlserver" => sqlserver(kind, &mut losses),
        "sqlite" => sqlite(kind, &mut losses),
        "duckdb" => duckdb(kind, &mut losses),
        "clickhouse" => {
            let inner = clickhouse(kind, &mut losses);
            if nullable {
                format!("Nullable({inner})")
            } else {
                inner
            }
        }
        // An engine a copy cannot write to has no types here; the copy refuses it before.
        _ => {
            losses.push(Loss::AsText {
                native: format!("{kind:?}"),
            });
            "TEXT".into()
        }
    };
    TargetType { sql_type, losses }
}

/// A decimal of `precision` and `scale` where the target allows at most `max_p` digits and
/// `max_s` places, written by `write`.
fn decimal(
    precision: Option<u32>,
    scale: Option<u32>,
    max_p: u32,
    max_s: u32,
    losses: &mut Vec<Loss>,
    write: impl Fn(u32, u32) -> String,
) -> String {
    match (precision, scale) {
        (Some(p), s) if p <= max_p && s.unwrap_or(0) <= max_s.min(p) => write(p, s.unwrap_or(0)),
        (Some(_), s) => {
            // Keep as many places as the source has, within the target's bounds.
            let scale = s.unwrap_or(0).min(max_s);
            losses.push(Loss::DecimalNarrowed {
                precision: max_p,
                scale,
            });
            write(max_p, scale)
        }
        (None, _) => {
            // No precision of its own: the widest, with half of it for places.
            let scale = (max_p / 2).min(max_s);
            losses.push(Loss::DecimalBounded {
                precision: max_p,
                scale,
            });
            write(max_p, scale)
        }
    }
}

fn other(kind: Portable, losses: &mut Vec<Loss>) {
    losses.push(match kind {
        Portable::Time => Loss::TimeAsText,
        Portable::Uuid => Loss::UuidAsText,
        Portable::Json => Loss::JsonAsText,
        Portable::Other => Loss::AsText {
            native: "its own type".into(),
        },
        Portable::Bool
        | Portable::Int { .. }
        | Portable::Decimal { .. }
        | Portable::Float { .. }
        | Portable::Text { .. }
        | Portable::Binary { .. }
        | Portable::Date
        | Portable::Timestamp
        | Portable::TimestampTz => return,
    });
}

fn postgres(kind: Portable, losses: &mut Vec<Loss>) -> String {
    match kind {
        Portable::Bool => "boolean".into(),
        Portable::Int {
            bytes: 1 | 2,
            unsigned: false,
        }
        | Portable::Int {
            bytes: 1,
            unsigned: true,
        } => "smallint".into(),
        Portable::Int {
            bytes: 4,
            unsigned: false,
        }
        | Portable::Int {
            bytes: 2,
            unsigned: true,
        } => "integer".into(),
        Portable::Int {
            bytes: 4,
            unsigned: true,
        }
        | Portable::Int {
            unsigned: false, ..
        } => "bigint".into(),
        // No unsigned 64-bit integer: an exact decimal holds it.
        Portable::Int { .. } => "numeric(20,0)".into(),
        Portable::Decimal { precision, scale } => match (precision, scale) {
            (Some(p), s) if p <= 1000 => format!("numeric({p},{})", s.unwrap_or(0)),
            _ => "numeric".into(),
        },
        Portable::Float { bytes: 4 } => "real".into(),
        Portable::Float { .. } => "double precision".into(),
        Portable::Text { length: Some(n) } if n <= 10_485_760 => format!("varchar({n})"),
        Portable::Text { .. } => "text".into(),
        Portable::Binary { .. } => "bytea".into(),
        Portable::Date => "date".into(),
        Portable::Time => "time".into(),
        Portable::Timestamp => "timestamp".into(),
        Portable::TimestampTz => "timestamptz".into(),
        Portable::Uuid => "uuid".into(),
        // `json` keeps the text as it came; `jsonb` would reorder it.
        Portable::Json => "json".into(),
        Portable::Other => {
            other(kind, losses);
            "text".into()
        }
    }
}

fn mysql(kind: Portable, losses: &mut Vec<Loss>) -> String {
    let int = |name: &str, unsigned: bool| {
        if unsigned {
            format!("{name} unsigned")
        } else {
            name.to_string()
        }
    };
    match kind {
        // MySQL's boolean is `tinyint(1)`, read back as a boolean.
        Portable::Bool => "tinyint(1)".into(),
        Portable::Int { bytes: 1, unsigned } => int("tinyint", unsigned),
        Portable::Int { bytes: 2, unsigned } => int("smallint", unsigned),
        Portable::Int { bytes: 4, unsigned } => int("int", unsigned),
        Portable::Int { unsigned, .. } => int("bigint", unsigned),
        Portable::Decimal { precision, scale } => {
            decimal(precision, scale, 65, 30, losses, |p, s| {
                format!("decimal({p},{s})")
            })
        }
        Portable::Float { bytes: 4 } => "float".into(),
        Portable::Float { .. } => "double".into(),
        Portable::Text { length: Some(n) } if n <= 16_383 => format!("varchar({n})"),
        Portable::Text { .. } => "longtext".into(),
        Portable::Binary { length } => {
            losses.push(Loss::BinaryNotYet);
            match length {
                Some(n) if n <= 65_535 => format!("varbinary({n})"),
                _ => "longblob".into(),
            }
        }
        Portable::Date => "date".into(),
        Portable::Time => "time(6)".into(),
        Portable::Timestamp => "datetime(6)".into(),
        Portable::TimestampTz => {
            losses.push(Loss::ZoneToUtc);
            "datetime(6)".into()
        }
        Portable::Uuid => {
            other(kind, losses);
            "char(36)".into()
        }
        Portable::Json => "json".into(),
        Portable::Other => {
            other(kind, losses);
            "longtext".into()
        }
    }
}

fn sqlserver(kind: Portable, losses: &mut Vec<Loss>) -> String {
    match kind {
        Portable::Bool => "bit".into(),
        // SQL Server's `tinyint` is unsigned: a signed byte goes to `smallint`.
        Portable::Int {
            bytes: 1,
            unsigned: true,
        } => "tinyint".into(),
        Portable::Int {
            bytes: 1 | 2,
            unsigned: false,
        } => "smallint".into(),
        Portable::Int {
            bytes: 2,
            unsigned: true,
        }
        | Portable::Int {
            bytes: 4,
            unsigned: false,
        } => "int".into(),
        Portable::Int {
            bytes: 4,
            unsigned: true,
        }
        | Portable::Int {
            unsigned: false, ..
        } => "bigint".into(),
        Portable::Int { .. } => "decimal(20,0)".into(),
        Portable::Decimal { precision, scale } => {
            decimal(precision, scale, 38, 38, losses, |p, s| {
                format!("decimal({p},{s})")
            })
        }
        Portable::Float { bytes: 4 } => "real".into(),
        Portable::Float { .. } => "float".into(),
        Portable::Text { length: Some(n) } if n <= 4_000 => format!("nvarchar({n})"),
        Portable::Text { .. } => "nvarchar(max)".into(),
        Portable::Binary { length } => {
            losses.push(Loss::BinaryNotYet);
            match length {
                Some(n) if n <= 8_000 => format!("varbinary({n})"),
                _ => "varbinary(max)".into(),
            }
        }
        Portable::Date => "date".into(),
        Portable::Time => "time(7)".into(),
        Portable::Timestamp => "datetime2(7)".into(),
        Portable::TimestampTz => "datetimeoffset(7)".into(),
        Portable::Uuid => "uniqueidentifier".into(),
        Portable::Json | Portable::Other => {
            other(kind, losses);
            "nvarchar(max)".into()
        }
    }
}

fn sqlite(kind: Portable, losses: &mut Vec<Loss>) -> String {
    match kind {
        Portable::Bool => "BOOLEAN".into(),
        Portable::Int { .. } => "INTEGER".into(),
        Portable::Decimal { precision, scale } => {
            losses.push(Loss::DecimalApproximate);
            match (precision, scale) {
                (Some(p), s) => format!("DECIMAL({p},{})", s.unwrap_or(0)),
                (None, _) => "NUMERIC".into(),
            }
        }
        Portable::Float { .. } => "REAL".into(),
        Portable::Text { length: Some(n) } => format!("VARCHAR({n})"),
        Portable::Text { .. } => "TEXT".into(),
        Portable::Binary { .. } => {
            losses.push(Loss::BinaryNotYet);
            "BLOB".into()
        }
        Portable::Date => "DATE".into(),
        Portable::Time => "TIME".into(),
        Portable::Timestamp => "TIMESTAMP".into(),
        Portable::TimestampTz => "TIMESTAMPTZ".into(),
        Portable::Uuid | Portable::Json | Portable::Other => {
            other(kind, losses);
            "TEXT".into()
        }
    }
}

fn duckdb(kind: Portable, losses: &mut Vec<Loss>) -> String {
    match kind {
        Portable::Bool => "BOOLEAN".into(),
        Portable::Int { bytes, unsigned } => match (bytes, unsigned) {
            (1, false) => "TINYINT",
            (2, false) => "SMALLINT",
            (4, false) => "INTEGER",
            (_, false) => "BIGINT",
            (1, true) => "UTINYINT",
            (2, true) => "USMALLINT",
            (4, true) => "UINTEGER",
            (_, true) => "UBIGINT",
        }
        .into(),
        Portable::Decimal { precision, scale } => {
            decimal(precision, scale, 38, 38, losses, |p, s| {
                format!("DECIMAL({p},{s})")
            })
        }
        Portable::Float { bytes: 4 } => "REAL".into(),
        Portable::Float { .. } => "DOUBLE".into(),
        Portable::Text { length: Some(n) } => format!("VARCHAR({n})"),
        Portable::Text { .. } => "VARCHAR".into(),
        Portable::Binary { .. } => "BLOB".into(),
        Portable::Date => "DATE".into(),
        Portable::Time => "TIME".into(),
        Portable::Timestamp => "TIMESTAMP".into(),
        Portable::TimestampTz => "TIMESTAMPTZ".into(),
        Portable::Uuid => "UUID".into(),
        Portable::Json => "JSON".into(),
        Portable::Other => {
            other(kind, losses);
            "VARCHAR".into()
        }
    }
}

fn clickhouse(kind: Portable, losses: &mut Vec<Loss>) -> String {
    match kind {
        Portable::Bool => "Bool".into(),
        Portable::Int { bytes, unsigned } => format!(
            "{}Int{}",
            if unsigned { "U" } else { "" },
            u32::from(bytes) * 8
        ),
        Portable::Decimal { precision, scale } => {
            decimal(precision, scale, 76, 76, losses, |p, s| {
                format!("Decimal({p}, {s})")
            })
        }
        Portable::Float { bytes: 4 } => "Float32".into(),
        Portable::Float { .. } => "Float64".into(),
        Portable::Text { length } => {
            if length.is_some() {
                losses.push(Loss::LengthDropped);
            }
            "String".into()
        }
        Portable::Binary { .. } => {
            losses.push(Loss::BinaryNotYet);
            "String".into()
        }
        Portable::Date => "Date32".into(),
        Portable::Timestamp => "DateTime64(6)".into(),
        Portable::TimestampTz => {
            losses.push(Loss::ZoneToUtc);
            "DateTime64(6)".into()
        }
        Portable::Uuid => "UUID".into(),
        Portable::Time | Portable::Json | Portable::Other => {
            other(kind, losses);
            "String".into()
        }
    }
}

#[cfg(test)]
#[path = "types_tests.rs"]
mod tests;
