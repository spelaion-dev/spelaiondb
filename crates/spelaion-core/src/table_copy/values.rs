//! Values on their way to the target (ADR-069): the core's canonical text of each value
//! (`Value`'s own: ISO dates, timestamps with their microseconds, decimals as written), with what
//! each target needs to read it besides. The import's normalizer then does what it does for any
//! imported text (booleans to `1`/`0` for `bit` columns, a zone to UTC for MySQL).

use std::fmt::Write as _;

use spelaion_driver_api::Value;

/// `value` as the text the target (a dialect's name) reads, `None` for NULL.
pub fn text_for(target: &str, value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        // Bytes as each engine reads them from text: PostgreSQL's `\x0102`, DuckDB's escape per
        // byte `\x01\x02`. Elsewhere a binary column is refused before (ADR-069's point 8): the
        // hex text would be stored as it is.
        Value::Bytes(bytes) => Some(match target {
            "duckdb" => {
                let mut text = String::with_capacity(bytes.len() * 4);
                for byte in bytes {
                    let _ = write!(text, "\\x{byte:02X}");
                }
                text
            }
            _ => value.to_string(),
        }),
        // `+00:00` rather than PostgreSQL's `+00`: every engine and the import's normalizer
        // read that form.
        Value::TimestampTz(_) => {
            let text = value.to_string();
            Some(match text.strip_suffix("+00") {
                Some(local) => format!("{local}+00:00"),
                None => text,
            })
        }
        // The shortest exact form, in exponent form where the plain one would be long (`1e300`
        // rather than 301 digits): every engine reads both.
        Value::Float(x) if x.is_finite() && *x != 0.0 && (x.abs() >= 1e16 || x.abs() < 1e-6) => {
            Some(format!("{x:e}"))
        }
        Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Decimal(_)
        | Value::Text(_)
        | Value::Date(_)
        | Value::Time(_)
        | Value::Timestamp(_)
        | Value::Uuid(_)
        | Value::Json(_)
        | Value::Interval(_)
        | Value::Array(_)
        | Value::Native(_) => Some(value.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_travel_as_canonical_text() {
        let cases = [
            (Value::Null, None),
            (Value::Bool(true), Some("true")),
            (
                Value::Int(-9_223_372_036_854_775_808),
                Some("-9223372036854775808"),
            ),
            (Value::Float(0.1), Some("0.1")),
            (Value::Float(1e300), Some("1e300")),
            (Value::Float(-2.5e-10), Some("-2.5e-10")),
            (Value::Float(123456.789), Some("123456.789")),
            (Value::Float(f64::MAX), Some("1.7976931348623157e308")),
            (
                Value::Decimal("-12345678901234567890.123456789".into()),
                Some("-12345678901234567890.123456789"),
            ),
            (
                Value::Text("ñandú 'x' \"y\"\n\t".into()),
                Some("ñandú 'x' \"y\"\n\t"),
            ),
            (Value::Date(19_723), Some("2024-01-01")),
            (Value::Time(37_230_500_000), Some("10:20:30.5")),
            (
                Value::Timestamp(1_704_103_230_123_456),
                Some("2024-01-01 10:00:30.123456"),
            ),
            (
                Value::TimestampTz(1_704_103_230_000_000),
                Some("2024-01-01 10:00:30+00:00"),
            ),
            (
                Value::Uuid("550e8400-e29b-41d4-a716-446655440000".into()),
                Some("550e8400-e29b-41d4-a716-446655440000"),
            ),
            (
                Value::Json("{\"a\": [1, 2]}".into()),
                Some("{\"a\": [1, 2]}"),
            ),
        ];
        for (value, expected) in cases {
            for target in [
                "postgres",
                "mysql",
                "sqlserver",
                "sqlite",
                "duckdb",
                "clickhouse",
            ] {
                assert_eq!(
                    text_for(target, &value).as_deref(),
                    expected,
                    "{target}: {value:?}"
                );
            }
        }
    }

    #[test]
    fn bytes_are_written_as_each_engine_reads_them() {
        let bytes = Value::Bytes(vec![0x00, 0x7f, 0xff]);
        assert_eq!(text_for("postgres", &bytes).as_deref(), Some("\\x007fff"));
        assert_eq!(
            text_for("duckdb", &bytes).as_deref(),
            Some("\\x00\\x7F\\xFF")
        );
    }
}
