//! Decoding of PostgreSQL's binary format into Arrow columns.
//!
//! Types with a common equivalent (integers, dates…) go into typed Arrow columns. Everything
//! else is converted to the same text `psql` would show, so nothing is lost: arrays, ranges,
//! composites, enums, domains and extension types.

use std::fmt::Write as _;
use std::sync::Arc;

use arrow::array::{
    ArrayRef, BinaryBuilder, BooleanBuilder, Date32Builder, Float64Builder, Int64Builder,
    StringBuilder, Time64MicrosecondBuilder, TimestampMicrosecondBuilder,
};
use fallible_iterator::FallibleIterator;
use postgres_protocol::types as pg;
use spelaion_driver_api::{LogicalType, Value};
use tokio_postgres::types::{Kind, Type};

pub type DecodeError = Box<dyn std::error::Error + Sync + Send>;
type DResult<T> = Result<T, DecodeError>;

/// OIDs of the built-in types (stable across all PostgreSQL versions).
mod oid {
    pub const BOOL: u32 = 16;
    pub const BYTEA: u32 = 17;
    pub const CHAR: u32 = 18;
    pub const NAME: u32 = 19;
    pub const INT8: u32 = 20;
    pub const INT2: u32 = 21;
    pub const INT4: u32 = 23;
    pub const TEXT: u32 = 25;
    pub const OID: u32 = 26;
    pub const JSON: u32 = 114;
    pub const XML: u32 = 142;
    pub const POINT: u32 = 600;
    pub const PATH: u32 = 602;
    pub const BOX: u32 = 603;
    pub const CIDR: u32 = 650;
    pub const FLOAT4: u32 = 700;
    pub const FLOAT8: u32 = 701;
    pub const UNKNOWN: u32 = 705;
    pub const MACADDR8: u32 = 774;
    pub const MONEY: u32 = 790;
    pub const MACADDR: u32 = 829;
    pub const INET: u32 = 869;
    pub const BPCHAR: u32 = 1042;
    pub const VARCHAR: u32 = 1043;
    pub const DATE: u32 = 1082;
    pub const TIME: u32 = 1083;
    pub const TIMESTAMP: u32 = 1114;
    pub const TIMESTAMPTZ: u32 = 1184;
    pub const INTERVAL: u32 = 1186;
    pub const TIMETZ: u32 = 1266;
    pub const BIT: u32 = 1560;
    pub const VARBIT: u32 = 1562;
    pub const NUMERIC: u32 = 1700;
    pub const UUID: u32 = 2950;
    pub const JSONB: u32 = 3802;
}

/// Days between 1970-01-01 and 2000-01-01 (the PostgreSQL epoch).
const PG_EPOCH_DAYS: i32 = 10_957;
const PG_EPOCH_MICROS: i64 = 946_684_800_000_000;

/// Effective type: domains are decoded as their base type.
fn base(ty: &Type) -> &Type {
    match ty.kind() {
        Kind::Domain(inner) => base(inner),
        _ => ty,
    }
}

pub fn logical_type(ty: &Type) -> LogicalType {
    let ty = base(ty);
    match ty.oid() {
        oid::BOOL => LogicalType::Bool,
        oid::INT2 | oid::INT4 | oid::INT8 | oid::OID => LogicalType::Int,
        oid::FLOAT4 | oid::FLOAT8 => LogicalType::Float,
        oid::NUMERIC | oid::MONEY => LogicalType::Decimal,
        oid::TEXT
        | oid::VARCHAR
        | oid::BPCHAR
        | oid::NAME
        | oid::CHAR
        | oid::XML
        | oid::UNKNOWN => LogicalType::Text,
        oid::BYTEA => LogicalType::Bytes,
        oid::DATE => LogicalType::Date,
        oid::TIME => LogicalType::Time,
        oid::TIMESTAMP => LogicalType::Timestamp,
        oid::TIMESTAMPTZ => LogicalType::TimestampTz,
        oid::UUID => LogicalType::Uuid,
        oid::JSON | oid::JSONB => LogicalType::Json,
        oid::INTERVAL => LogicalType::Interval,
        _ => match ty.kind() {
            Kind::Array(_) => LogicalType::Array,
            Kind::Enum(_) => LogicalType::Text,
            _ => LogicalType::Native,
        },
    }
}

/// Accumulates a column's values until a batch is complete.
pub enum ColumnBuilder {
    Bool(BooleanBuilder),
    Int(Int64Builder, u32),
    Float(Float64Builder, u32),
    Bytes(BinaryBuilder),
    Date(Date32Builder),
    Time(Time64MicrosecondBuilder),
    Timestamp(TimestampMicrosecondBuilder),
    TimestampTz(TimestampMicrosecondBuilder),
    Text(StringBuilder, Type),
}

impl ColumnBuilder {
    pub fn new(ty: &Type, capacity: usize) -> Self {
        let b = base(ty);
        match logical_type(ty) {
            LogicalType::Bool => Self::Bool(BooleanBuilder::with_capacity(capacity)),
            LogicalType::Int => Self::Int(Int64Builder::with_capacity(capacity), b.oid()),
            LogicalType::Float => Self::Float(Float64Builder::with_capacity(capacity), b.oid()),
            LogicalType::Bytes => {
                Self::Bytes(BinaryBuilder::with_capacity(capacity, capacity * 16))
            }
            LogicalType::Date => Self::Date(Date32Builder::with_capacity(capacity)),
            LogicalType::Time => Self::Time(Time64MicrosecondBuilder::with_capacity(capacity)),
            LogicalType::Timestamp => {
                Self::Timestamp(TimestampMicrosecondBuilder::with_capacity(capacity))
            }
            LogicalType::TimestampTz => Self::TimestampTz(
                TimestampMicrosecondBuilder::with_capacity(capacity).with_timezone("UTC"),
            ),
            LogicalType::Decimal
            | LogicalType::Text
            | LogicalType::Uuid
            | LogicalType::Json
            | LogicalType::Interval
            | LogicalType::Array
            | LogicalType::Native => Self::Text(
                StringBuilder::with_capacity(capacity, capacity * 16),
                b.clone(),
            ),
        }
    }

    pub fn append(&mut self, raw: Option<&[u8]>) -> DResult<()> {
        let Some(raw) = raw else {
            match self {
                Self::Bool(b) => b.append_null(),
                Self::Int(b, _) => b.append_null(),
                Self::Float(b, _) => b.append_null(),
                Self::Bytes(b) => b.append_null(),
                Self::Date(b) => b.append_null(),
                Self::Time(b) => b.append_null(),
                Self::Timestamp(b) | Self::TimestampTz(b) => b.append_null(),
                Self::Text(b, _) => b.append_null(),
            }
            return Ok(());
        };
        match self {
            Self::Bool(b) => b.append_value(pg::bool_from_sql(raw)?),
            Self::Int(b, oid) => b.append_value(decode_int(*oid, raw)?),
            Self::Float(b, oid) => b.append_value(decode_float(*oid, raw)?),
            Self::Bytes(b) => b.append_value(raw),
            Self::Date(b) => b.append_value(decode_date(raw)?),
            Self::Time(b) => b.append_value(pg::time_from_sql(raw)?),
            Self::Timestamp(b) | Self::TimestampTz(b) => b.append_value(decode_timestamp(raw)?),
            Self::Text(b, ty) => b.append_value(decode_text(ty, raw)?),
        }
        Ok(())
    }

    pub fn finish(&mut self) -> ArrayRef {
        match self {
            Self::Bool(b) => Arc::new(b.finish()),
            Self::Int(b, _) => Arc::new(b.finish()),
            Self::Float(b, _) => Arc::new(b.finish()),
            Self::Bytes(b) => Arc::new(b.finish()),
            Self::Date(b) => Arc::new(b.finish()),
            Self::Time(b) => Arc::new(b.finish()),
            Self::Timestamp(b) | Self::TimestampTz(b) => Arc::new(b.finish()),
            Self::Text(b, _) => Arc::new(b.finish()),
        }
    }
}

fn decode_int(oid: u32, raw: &[u8]) -> DResult<i64> {
    Ok(match oid {
        oid::INT2 => pg::int2_from_sql(raw)?.into(),
        oid::INT4 => pg::int4_from_sql(raw)?.into(),
        oid::OID => pg::oid_from_sql(raw)?.into(),
        _ => pg::int8_from_sql(raw)?,
    })
}

fn decode_float(oid: u32, raw: &[u8]) -> DResult<f64> {
    Ok(match oid {
        // Via the shortest text: 0.1f32 must stay 0.1, not 0.10000000149011612.
        oid::FLOAT4 => pg::float4_from_sql(raw)?.to_string().parse()?,
        _ => pg::float8_from_sql(raw)?,
    })
}

fn decode_date(raw: &[u8]) -> DResult<i32> {
    Ok(match pg::date_from_sql(raw)? {
        d @ (i32::MAX | i32::MIN) => d, // ±infinity
        d => d.checked_add(PG_EPOCH_DAYS).ok_or("date out of range")?,
    })
}

fn decode_timestamp(raw: &[u8]) -> DResult<i64> {
    Ok(match pg::timestamp_from_sql(raw)? {
        t @ (i64::MAX | i64::MIN) => t, // ±infinity
        t => t
            .checked_add(PG_EPOCH_MICROS)
            .ok_or("timestamp out of range")?,
    })
}

/// Text of a value, exactly as `psql` would show it.
pub fn decode_text(ty: &Type, raw: &[u8]) -> DResult<String> {
    let ty = base(ty);
    Ok(match ty.oid() {
        oid::TEXT
        | oid::VARCHAR
        | oid::BPCHAR
        | oid::NAME
        | oid::XML
        | oid::UNKNOWN
        | oid::JSON => pg::text_from_sql(raw)?.to_string(),
        oid::JSONB => {
            // First byte: format version (1).
            let (_version, body) = raw.split_first().ok_or("empty jsonb")?;
            std::str::from_utf8(body)?.to_string()
        }
        oid::CHAR => {
            let c = pg::char_from_sql(raw)? as u8;
            if c == 0 {
                String::new()
            } else {
                (c as char).to_string()
            }
        }
        // Inside arrays and composites, like PostgreSQL (`{t,f}`), so the text can be pasted
        // back as is. Standalone bool cells do not go through here.
        oid::BOOL => if pg::bool_from_sql(raw)? { "t" } else { "f" }.to_string(),
        oid::INT2 | oid::INT4 | oid::INT8 | oid::OID => decode_int(ty.oid(), raw)?.to_string(),
        oid::FLOAT4 | oid::FLOAT8 => Value::Float(decode_float(ty.oid(), raw)?).to_string(),
        oid::NUMERIC => numeric_text(raw)?,
        oid::MONEY => money_text(pg::int8_from_sql(raw)?),
        oid::BYTEA => Value::Bytes(raw.to_vec()).to_string(),
        oid::DATE => Value::Date(decode_date(raw)?).to_string(),
        oid::TIME => Value::Time(pg::time_from_sql(raw)?).to_string(),
        oid::TIMESTAMP => Value::Timestamp(decode_timestamp(raw)?).to_string(),
        oid::TIMESTAMPTZ => Value::TimestampTz(decode_timestamp(raw)?).to_string(),
        oid::TIMETZ => timetz_text(raw)?,
        oid::INTERVAL => interval_text(raw)?,
        oid::UUID => uuid_text(&pg::uuid_from_sql(raw)?),
        oid::INET | oid::CIDR => {
            let inet = pg::inet_from_sql(raw)?;
            let full = if inet.addr().is_ipv4() { 32 } else { 128 };
            if ty.oid() == oid::INET && inet.netmask() == full {
                inet.addr().to_string()
            } else {
                format!("{}/{}", inet.addr(), inet.netmask())
            }
        }
        oid::MACADDR | oid::MACADDR8 => raw
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
        oid::BIT | oid::VARBIT => {
            let bits = pg::varbit_from_sql(raw)?;
            (0..bits.len())
                .map(|i| {
                    if bits.bytes()[i / 8] & (0x80 >> (i % 8)) != 0 {
                        '1'
                    } else {
                        '0'
                    }
                })
                .collect()
        }
        oid::POINT => {
            let p = pg::point_from_sql(raw)?;
            format!("({},{})", p.x(), p.y())
        }
        oid::BOX => {
            let b = pg::box_from_sql(raw)?;
            let (ur, ll) = (b.upper_right(), b.lower_left());
            format!("({},{}),({},{})", ur.x(), ur.y(), ll.x(), ll.y())
        }
        oid::PATH => {
            let path = pg::path_from_sql(raw)?;
            let points: Vec<String> = path
                .points()
                .map(|p| Ok(format!("({},{})", p.x(), p.y())))
                .collect()?;
            let (open, close) = if path.closed() {
                ('(', ')')
            } else {
                ('[', ']')
            };
            format!("{open}{}{close}", points.join(","))
        }
        _ => match ty.kind() {
            Kind::Array(elem) => array_text(elem, raw)?,
            Kind::Range(elem) => range_text(elem, raw)?,
            Kind::Multirange(elem) => multirange_text(elem, raw)?,
            Kind::Composite(fields) => composite_text(fields, raw)?,
            Kind::Enum(_) => pg::text_from_sql(raw)?.to_string(),
            _ => extension_text(ty, raw)?,
        },
    })
}

/// Extension and unknown types.
fn extension_text(ty: &Type, raw: &[u8]) -> DResult<String> {
    Ok(match ty.name() {
        "hstore" => {
            let pairs: Vec<String> = pg::hstore_from_sql(raw)?
                .map(|(k, v)| {
                    Ok(match v {
                        Some(v) => format!("\"{}\"=>\"{}\"", escape_quoted(k), escape_quoted(v)),
                        None => format!("\"{}\"=>NULL", escape_quoted(k)),
                    })
                })
                .collect()?;
            pairs.join(", ")
        }
        "ltree" | "lquery" | "ltxtquery" => {
            let (_version, body) = raw.split_first().ok_or("empty ltree")?;
            std::str::from_utf8(body)?.to_string()
        }
        // PostGIS: psql shows the EWKB as uppercase hexadecimal.
        "geometry" | "geography" => raw.iter().map(|b| format!("{b:02X}")).collect(),
        _ => match std::str::from_utf8(raw) {
            // Many extension types (citext…) send their text as is.
            Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => s.to_string(),
            _ => Value::Bytes(raw.to_vec()).to_string(),
        },
    })
}

fn escape_quoted(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Quotes an array or composite element when needed, like `psql`.
fn quote_element(s: &str, extra_special: &[char], quote_null_word: bool) -> String {
    let needs = s.is_empty()
        || (quote_null_word && s.eq_ignore_ascii_case("null"))
        || s.chars()
            .any(|c| c == '"' || c == '\\' || c.is_whitespace() || extra_special.contains(&c));
    if needs {
        format!("\"{}\"", escape_quoted(s))
    } else {
        s.to_string()
    }
}

fn array_text(elem: &Type, raw: &[u8]) -> DResult<String> {
    // PostgreSQL's MAXDIM is six. Check the untrusted wire header before the protocol
    // parser scans dimensions and before rendering recurses once per dimension.
    const MAX_ARRAY_DIMENSIONS: i32 = 6;
    let mut header = raw;
    let rank = read_i32(&mut header)?;
    if !(0..=MAX_ARRAY_DIMENSIONS).contains(&rank) {
        return Err("invalid PostgreSQL array dimension count".into());
    }
    let array = pg::array_from_sql(raw)?;
    let dims: Vec<pg::ArrayDimension> = array.dimensions().collect()?;
    let values: Vec<Option<&[u8]>> = array.values().collect()?;
    if dims.is_empty() {
        return Ok("{}".to_string());
    }
    let mut out = String::new();
    if dims.iter().any(|d| d.lower_bound != 1) {
        for d in &dims {
            write!(out, "[{}:{}]", d.lower_bound, d.lower_bound + d.len - 1)?;
        }
        out.push('=');
    }
    let mut next = 0usize;
    write_array_level(&mut out, elem, &dims, &values, &mut next)?;
    Ok(out)
}

fn write_array_level(
    out: &mut String,
    elem: &Type,
    dims: &[pg::ArrayDimension],
    values: &[Option<&[u8]>],
    next: &mut usize,
) -> DResult<()> {
    out.push('{');
    for i in 0..dims[0].len {
        if i > 0 {
            out.push(',');
        }
        if dims.len() > 1 {
            write_array_level(out, elem, &dims[1..], values, next)?;
        } else {
            match values.get(*next).copied().flatten() {
                Some(v) => out.push_str(&quote_element(
                    &decode_text(elem, v)?,
                    &['{', '}', ','],
                    true,
                )),
                None => out.push_str("NULL"),
            }
            *next += 1;
        }
    }
    out.push('}');
    Ok(())
}

fn range_text(elem: &Type, raw: &[u8]) -> DResult<String> {
    use pg::{Range, RangeBound};
    let bound = |b: &RangeBound<Option<&[u8]>>| -> DResult<(bool, String)> {
        Ok(match b {
            RangeBound::Inclusive(v) | RangeBound::Exclusive(v) => {
                let inclusive = matches!(b, RangeBound::Inclusive(_));
                let text = match v {
                    Some(v) => {
                        quote_element(&decode_text(elem, v)?, &['(', ')', '[', ']', ','], false)
                    }
                    None => String::new(),
                };
                (inclusive, text)
            }
            RangeBound::Unbounded => (false, String::new()),
        })
    };
    Ok(match pg::range_from_sql(raw)? {
        Range::Empty => "empty".to_string(),
        Range::Nonempty(lower, upper) => {
            let (li, lt) = bound(&lower)?;
            let (ui, ut) = bound(&upper)?;
            format!(
                "{}{lt},{ut}{}",
                if li { '[' } else { '(' },
                if ui { ']' } else { ')' }
            )
        }
    })
}

/// `Kind::Multirange` carries the element type (int4), not the range type (int4range).
fn multirange_text(elem: &Type, mut raw: &[u8]) -> DResult<String> {
    let count = usize::try_from(read_i32(&mut raw)?)?;
    // Each range has at least a four-byte length and a one-byte flags field.
    if count > raw.len() / 5 {
        return Err("invalid multirange count".into());
    }
    let mut parts = Vec::new();
    parts.try_reserve(count)?;
    for _ in 0..count {
        let len = usize::try_from(read_i32(&mut raw)?)?;
        let (item, rest) = raw.split_at_checked(len).ok_or("truncated multirange")?;
        parts.push(range_text(elem, item)?);
        raw = rest;
    }
    Ok(format!("{{{}}}", parts.join(",")))
}

fn composite_text(fields: &[tokio_postgres::types::Field], mut raw: &[u8]) -> DResult<String> {
    let count = usize::try_from(read_i32(&mut raw)?)?;
    if count != fields.len() || count > raw.len() / 8 {
        return Err("invalid composite field count".into());
    }
    let mut parts = Vec::new();
    parts.try_reserve(count)?;
    for field in fields {
        let _oid = read_i32(&mut raw)?;
        let len = read_i32(&mut raw)?;
        if len == -1 {
            parts.push(String::new());
            continue;
        }
        let len = usize::try_from(len)?;
        let (value, rest) = raw.split_at_checked(len).ok_or("truncated composite")?;
        parts.push(quote_element(
            &decode_text(field.type_(), value)?,
            &['(', ')', ','],
            false,
        ));
        raw = rest;
    }
    Ok(format!("({})", parts.join(",")))
}

fn read_i32(raw: &mut &[u8]) -> DResult<i32> {
    let (head, rest) = raw.split_first_chunk::<4>().ok_or("truncated value")?;
    *raw = rest;
    Ok(i32::from_be_bytes(*head))
}

fn read_i16(raw: &mut &[u8]) -> DResult<i16> {
    let (head, rest) = raw.split_first_chunk::<2>().ok_or("truncated value")?;
    *raw = rest;
    Ok(i16::from_be_bytes(*head))
}

fn read_i64(raw: &mut &[u8]) -> DResult<i64> {
    let (head, rest) = raw.split_first_chunk::<8>().ok_or("truncated value")?;
    *raw = rest;
    Ok(i64::from_be_bytes(*head))
}

/// Binary NUMERIC: base-10 000 digits with weight and scale.
pub fn numeric_text(mut raw: &[u8]) -> DResult<String> {
    let ndigits = read_i16(&mut raw)?;
    let weight = read_i16(&mut raw)?;
    let sign = read_i16(&mut raw)? as u16;
    let dscale = read_i16(&mut raw)?;
    let digits: Vec<i16> = (0..ndigits)
        .map(|_| read_i16(&mut raw))
        .collect::<DResult<_>>()?;
    let digit = |i: i32| -> i16 {
        if i >= 0 {
            digits.get(i as usize).copied().unwrap_or(0)
        } else {
            0
        }
    };

    match sign {
        0xC000 => return Ok("NaN".into()),
        0xD000 => return Ok("Infinity".into()),
        0xF000 => return Ok("-Infinity".into()),
        _ => {}
    }
    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        for i in 0..=i32::from(weight) {
            if i == 0 {
                write!(out, "{}", digit(i))?;
            } else {
                write!(out, "{:04}", digit(i))?;
            }
        }
    }
    if dscale > 0 {
        let mut frac = String::new();
        let mut i = i32::from(weight) + 1;
        while frac.len() < dscale as usize {
            write!(frac, "{:04}", digit(i))?;
            i += 1;
        }
        frac.truncate(dscale as usize);
        out.push('.');
        out.push_str(&frac);
    }
    Ok(out)
}

/// `money` is stored in cents. The symbol and separators depend on the server's
/// `lc_monetary`; here the number is returned without regional formatting.
fn money_text(cents: i64) -> String {
    let sign = if cents < 0 { "-" } else { "" };
    let abs = cents.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

fn uuid_text(b: &[u8; 16]) -> String {
    let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

fn timetz_text(mut raw: &[u8]) -> DResult<String> {
    let micros = read_i64(&mut raw)?;
    // PostgreSQL stores the zone in seconds WEST of UTC.
    let offset = -read_i32(&mut raw)?;
    let mut out = Value::Time(micros).to_string();
    let abs = offset.unsigned_abs();
    write!(
        out,
        "{}{:02}",
        if offset < 0 { '-' } else { '+' },
        abs / 3600
    )?;
    if abs % 3600 != 0 {
        write!(out, ":{:02}", abs / 60 % 60)?;
    }
    if abs % 60 != 0 {
        write!(out, ":{:02}", abs % 60)?;
    }
    Ok(out)
}

/// INTERVAL in PostgreSQL's default output style
/// (`1 year 2 mons 3 days 04:05:06.5`).
pub fn interval_text(mut raw: &[u8]) -> DResult<String> {
    let micros = read_i64(&mut raw)?;
    let days = read_i32(&mut raw)?;
    let months = read_i32(&mut raw)?;
    let (years, mons) = (months / 12, months % 12);

    let mut parts: Vec<String> = Vec::new();
    let mut seen_negative = false;
    let mut unit = |n: i32, singular: &str, plural: &str, parts: &mut Vec<String>| {
        if n == 0 {
            return;
        }
        let sign = if n > 0 && seen_negative { "+" } else { "" };
        seen_negative |= n < 0;
        parts.push(format!(
            "{sign}{n} {}",
            if n == 1 { singular } else { plural }
        ));
    };
    unit(years, "year", "years", &mut parts);
    unit(mons, "mon", "mons", &mut parts);
    unit(days, "day", "days", &mut parts);

    if micros != 0 || parts.is_empty() {
        let sign = if micros < 0 {
            "-"
        } else if seen_negative {
            "+"
        } else {
            ""
        };
        let abs = micros.unsigned_abs();
        let secs = abs / 1_000_000;
        let mut time = format!(
            "{sign}{:02}:{:02}:{:02}",
            secs / 3600,
            secs / 60 % 60,
            secs % 60
        );
        let frac = abs % 1_000_000;
        if frac != 0 {
            write!(time, ".{}", format!("{frac:06}").trim_end_matches('0'))?;
        }
        parts.push(time);
    }
    Ok(parts.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composite_and_multirange_reject_untrusted_counts() {
        let fields = vec![tokio_postgres::types::Field::new("n".into(), Type::INT4)];
        for count in [-1_i32, i32::MAX] {
            assert!(composite_text(&fields, &count.to_be_bytes()).is_err());
            assert!(multirange_text(&Type::INT4, &count.to_be_bytes()).is_err());
        }
        // A null field still counts towards the declared type's field count.
        let mut extra_null = 2_i32.to_be_bytes().to_vec();
        for _ in 0..2 {
            extra_null.extend_from_slice(&23_i32.to_be_bytes());
            extra_null.extend_from_slice(&(-1_i32).to_be_bytes());
        }
        assert!(composite_text(&fields, &extra_null).is_err());
        assert!(composite_text(&fields, &0_i32.to_be_bytes()).is_err());
        let mut one_null = 1_i32.to_be_bytes().to_vec();
        one_null.extend_from_slice(&23_i32.to_be_bytes());
        one_null.extend_from_slice(&(-1_i32).to_be_bytes());
        assert_eq!(composite_text(&fields, &one_null).unwrap(), "()");
        one_null[8..12].copy_from_slice(&(-2_i32).to_be_bytes());
        assert!(composite_text(&fields, &one_null).is_err());

        assert_eq!(
            multirange_text(&Type::INT4, &0_i32.to_be_bytes()).unwrap(),
            "{}"
        );
        let mut one_empty = 1_i32.to_be_bytes().to_vec();
        one_empty.extend_from_slice(&1_i32.to_be_bytes());
        one_empty.push(1); // RANGE_EMPTY
        assert_eq!(multirange_text(&Type::INT4, &one_empty).unwrap(), "{empty}");
        one_empty[4..8].copy_from_slice(&(-1_i32).to_be_bytes());
        assert!(multirange_text(&Type::INT4, &one_empty).is_err());
    }

    fn int_array(rank: i32) -> Vec<u8> {
        let mut raw = Vec::new();
        for word in [rank, 0, 23] {
            raw.extend_from_slice(&word.to_be_bytes());
        }
        for _ in 0..rank {
            raw.extend_from_slice(&1_i32.to_be_bytes());
            raw.extend_from_slice(&1_i32.to_be_bytes());
        }
        raw.extend_from_slice(&4_i32.to_be_bytes());
        raw.extend_from_slice(&42_i32.to_be_bytes());
        raw
    }

    #[test]
    fn array_rank_is_limited_to_postgresql_maximum() {
        assert!(decode_text(&Type::INT4_ARRAY, &int_array(7)).is_err());
        assert!(decode_text(&Type::INT4_ARRAY, &i32::MAX.to_be_bytes()).is_err());
        assert!(decode_text(&Type::INT4_ARRAY, &(-1_i32).to_be_bytes()).is_err());
        assert!(decode_text(&Type::INT4_ARRAY, &[0, 0, 0]).is_err());
        let mut empty = int_array(0);
        empty.truncate(12);
        assert_eq!(decode_text(&Type::INT4_ARRAY, &empty).unwrap(), "{}");
        assert_eq!(
            decode_text(&Type::INT4_ARRAY, &int_array(6)).unwrap(),
            "{{{{{{42}}}}}}"
        );
        assert_eq!(
            decode_text(&Type::INT4_ARRAY, &int_array(1)).unwrap(),
            "{42}"
        );
    }

    fn numeric(ndigits: i16, weight: i16, sign: u16, dscale: i16, digits: &[i16]) -> Vec<u8> {
        let mut v = Vec::new();
        for x in [ndigits, weight, sign as i16, dscale].iter().chain(digits) {
            v.extend_from_slice(&x.to_be_bytes());
        }
        v
    }

    #[test]
    fn numeric_binary_to_text() {
        assert_eq!(
            numeric_text(&numeric(2, 0, 0, 2, &[1234, 5600])).unwrap(),
            "1234.56"
        );
        assert_eq!(
            numeric_text(&numeric(1, -1, 0x4000, 4, &[5])).unwrap(),
            "-0.0005"
        );
        assert_eq!(
            numeric_text(&numeric(1, 2, 0, 0, &[7])).unwrap(),
            "700000000"
        );
        assert_eq!(numeric_text(&numeric(0, 0, 0, 3, &[])).unwrap(), "0.000");
        assert_eq!(numeric_text(&numeric(0, 0, 0xC000, 0, &[])).unwrap(), "NaN");
    }

    fn interval(micros: i64, days: i32, months: i32) -> Vec<u8> {
        let mut v = micros.to_be_bytes().to_vec();
        v.extend_from_slice(&days.to_be_bytes());
        v.extend_from_slice(&months.to_be_bytes());
        v
    }

    #[test]
    fn interval_binary_to_text() {
        assert_eq!(
            interval_text(&interval(14_706_500_000, 3, 14)).unwrap(),
            "1 year 2 mons 3 days 04:05:06.5"
        );
        assert_eq!(interval_text(&interval(0, 0, 0)).unwrap(), "00:00:00");
        assert_eq!(
            interval_text(&interval(7_200_000_000, -1, 0)).unwrap(),
            "-1 days +02:00:00"
        );
        assert_eq!(
            interval_text(&interval(-1, 0, 0)).unwrap(),
            "-00:00:00.000001"
        );
        assert_eq!(interval_text(&interval(0, 1, 0)).unwrap(), "1 day");
    }

    #[test]
    fn money_and_uuid() {
        assert_eq!(money_text(-123_456), "-1234.56");
        assert_eq!(money_text(5), "0.05");
        assert_eq!(
            uuid_text(&[0xab; 16]),
            "abababab-abab-abab-abab-abababababab"
        );
    }
}
