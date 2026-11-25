//! Column types across engines (ADR-069): what each engine's types read as, what each kind
//! becomes on each target, the losses named, and every target type one the import accepts.
use super::*;
use crate::table_copy::{SourceColumn, TARGETS, plan_column};
use LogicalType as L;

fn int(bytes: u8, unsigned: bool) -> Portable {
    Portable::Int { bytes, unsigned }
}

fn dec(p: u32, s: u32) -> Portable {
    Portable::Decimal {
        precision: Some(p),
        scale: Some(s),
    }
}

#[test]
fn each_engines_types_read_as_their_kind() {
    let cases: &[(&str, &str, LogicalType, Portable)] = &[
        // PostgreSQL.
        ("postgres", "boolean", L::Bool, Portable::Bool),
        ("postgres", "smallint", L::Int, int(2, false)),
        ("postgres", "integer", L::Int, int(4, false)),
        ("postgres", "bigint", L::Int, int(8, false)),
        ("postgres", "int8", L::Int, int(8, false)),
        ("postgres", "numeric(12,2)", L::Decimal, dec(12, 2)),
        (
            "postgres",
            "numeric",
            L::Decimal,
            Portable::Decimal {
                precision: None,
                scale: None,
            },
        ),
        ("postgres", "real", L::Float, Portable::Float { bytes: 4 }),
        (
            "postgres",
            "double precision",
            L::Float,
            Portable::Float { bytes: 8 },
        ),
        (
            "postgres",
            "character varying(80)",
            L::Text,
            Portable::Text { length: Some(80) },
        ),
        ("postgres", "text", L::Text, Portable::Text { length: None }),
        (
            "postgres",
            "bytea",
            L::Bytes,
            Portable::Binary { length: None },
        ),
        ("postgres", "date", L::Date, Portable::Date),
        (
            "postgres",
            "time without time zone",
            L::Time,
            Portable::Time,
        ),
        (
            "postgres",
            "timestamp without time zone",
            L::Timestamp,
            Portable::Timestamp,
        ),
        (
            "postgres",
            "timestamp with time zone",
            L::TimestampTz,
            Portable::TimestampTz,
        ),
        ("postgres", "uuid", L::Uuid, Portable::Uuid),
        ("postgres", "jsonb", L::Json, Portable::Json),
        ("postgres", "interval", L::Interval, Portable::Other),
        ("postgres", "integer[]", L::Array, Portable::Other),
        // MySQL and MariaDB.
        ("mysql", "tinyint(1)", L::Bool, Portable::Bool),
        ("mysql", "tinyint", L::Int, int(1, false)),
        ("mysql", "tinyint unsigned", L::Int, int(1, true)),
        ("mysql", "mediumint", L::Int, int(4, false)),
        ("mysql", "int(11) unsigned", L::Int, int(4, true)),
        ("mysql", "bigint unsigned", L::Int, int(8, true)),
        ("mysql", "decimal(65,30)", L::Decimal, dec(65, 30)),
        ("mysql", "float", L::Float, Portable::Float { bytes: 4 }),
        ("mysql", "double", L::Float, Portable::Float { bytes: 8 }),
        (
            "mysql",
            "varchar(255)",
            L::Text,
            Portable::Text { length: Some(255) },
        ),
        (
            "mysql",
            "longtext",
            L::Text,
            Portable::Text { length: None },
        ),
        (
            "mysql",
            "varbinary(16)",
            L::Bytes,
            Portable::Binary { length: Some(16) },
        ),
        ("mysql", "datetime(6)", L::Timestamp, Portable::Timestamp),
        ("mysql", "json", L::Json, Portable::Json),
        // SQL Server.
        ("sqlserver", "bit", L::Bool, Portable::Bool),
        ("sqlserver", "tinyint", L::Int, int(1, true)),
        ("sqlserver", "int", L::Int, int(4, false)),
        ("sqlserver", "money", L::Decimal, dec(19, 4)),
        ("sqlserver", "float", L::Float, Portable::Float { bytes: 8 }),
        (
            "sqlserver",
            "nvarchar(40)",
            L::Text,
            Portable::Text { length: Some(40) },
        ),
        (
            "sqlserver",
            "nvarchar(max)",
            L::Text,
            Portable::Text { length: None },
        ),
        (
            "sqlserver",
            "datetimeoffset(7)",
            L::TimestampTz,
            Portable::TimestampTz,
        ),
        ("sqlserver", "uniqueidentifier", L::Uuid, Portable::Uuid),
        // SQLite: 64-bit integers whatever their name.
        ("sqlite", "INTEGER", L::Int, int(8, false)),
        ("sqlite", "INT", L::Int, int(8, false)),
        ("sqlite", "TEXT", L::Text, Portable::Text { length: None }),
        ("sqlite", "REAL", L::Float, Portable::Float { bytes: 8 }),
        // DuckDB.
        ("duckdb", "UTINYINT", L::Int, int(1, true)),
        ("duckdb", "UBIGINT", L::Int, int(8, true)),
        ("duckdb", "INTEGER", L::Int, int(4, false)),
        ("duckdb", "DECIMAL(18,3)", L::Decimal, dec(18, 3)),
        (
            "duckdb",
            "TIMESTAMP WITH TIME ZONE",
            L::TimestampTz,
            Portable::TimestampTz,
        ),
        // ClickHouse: sizes in the names, wrappers looked through.
        ("clickhouse", "Int8", L::Int, int(1, false)),
        ("clickhouse", "UInt16", L::Int, int(2, true)),
        ("clickhouse", "Nullable(Int32)", L::Int, int(4, false)),
        ("clickhouse", "UInt64", L::Int, int(8, true)),
        ("clickhouse", "Decimal(38, 10)", L::Decimal, dec(38, 10)),
        ("clickhouse", "Decimal64(4)", L::Decimal, dec(18, 4)),
        (
            "clickhouse",
            "LowCardinality(Nullable(String))",
            L::Text,
            Portable::Text { length: None },
        ),
        (
            "clickhouse",
            "FixedString(8)",
            L::Text,
            Portable::Text { length: Some(8) },
        ),
        (
            "clickhouse",
            "Float32",
            L::Float,
            Portable::Float { bytes: 4 },
        ),
        (
            "clickhouse",
            "DateTime64(6)",
            L::Timestamp,
            Portable::Timestamp,
        ),
    ];
    for (engine, native, logical, expected) in cases {
        assert_eq!(
            portable(engine, native, *logical),
            *expected,
            "{engine} {native}"
        );
    }
}

/// Every kind, as a source can give it.
fn kinds() -> Vec<Portable> {
    vec![
        Portable::Bool,
        int(1, false),
        int(1, true),
        int(2, false),
        int(2, true),
        int(4, false),
        int(4, true),
        int(8, false),
        int(8, true),
        dec(12, 2),
        dec(76, 10),
        Portable::Decimal {
            precision: None,
            scale: None,
        },
        Portable::Float { bytes: 4 },
        Portable::Float { bytes: 8 },
        Portable::Text { length: Some(80) },
        Portable::Text { length: None },
        Portable::Binary { length: Some(16) },
        Portable::Binary { length: None },
        Portable::Date,
        Portable::Time,
        Portable::Timestamp,
        Portable::TimestampTz,
        Portable::Uuid,
        Portable::Json,
        Portable::Other,
    ]
}

/// Every target type is one an import accepts as typed (words, digits, balanced lists), so the
/// copy's `CREATE TABLE` goes through the same check.
#[test]
fn every_target_type_is_one_the_import_accepts() {
    for target in TARGETS {
        for kind in kinds() {
            for nullable in [false, true] {
                let mapped = target_type(target, kind, nullable);
                assert!(
                    crate::import::check_sql_type(&mapped.sql_type).is_ok(),
                    "{target} {kind:?}: {}",
                    mapped.sql_type
                );
            }
        }
    }
}

#[test]
fn each_target_gets_its_own_types_and_says_what_is_lost() {
    let check = |target: &str, kind: Portable, sql: &str, losses: &[Loss]| {
        let mapped = target_type(target, kind, false);
        assert_eq!(mapped.sql_type, sql, "{target} {kind:?}");
        assert_eq!(mapped.losses, losses, "{target} {kind:?}");
    };
    // PostgreSQL keeps everything but what has no counterpart.
    check("postgres", int(8, true), "numeric(20,0)", &[]);
    check("postgres", int(4, true), "bigint", &[]);
    check("postgres", dec(76, 10), "numeric(76,10)", &[]);
    check("postgres", Portable::Json, "json", &[]);
    check(
        "postgres",
        Portable::Other,
        "text",
        &[Loss::AsText {
            native: "its own type".into(),
        }],
    );
    check("postgres", Portable::Binary { length: None }, "bytea", &[]);
    // MySQL: no zone, no UUID, decimals up to 65 digits, binary not yet.
    check(
        "mysql",
        Portable::TimestampTz,
        "datetime(6)",
        &[Loss::ZoneToUtc],
    );
    check("mysql", Portable::Uuid, "char(36)", &[Loss::UuidAsText]);
    check("mysql", int(8, true), "bigint unsigned", &[]);
    check(
        "mysql",
        dec(76, 10),
        "decimal(65,10)",
        &[Loss::DecimalNarrowed {
            precision: 65,
            scale: 10,
        }],
    );
    check(
        "mysql",
        Portable::Decimal {
            precision: None,
            scale: None,
        },
        "decimal(65,30)",
        &[Loss::DecimalBounded {
            precision: 65,
            scale: 30,
        }],
    );
    check(
        "mysql",
        Portable::Binary { length: Some(16) },
        "varbinary(16)",
        &[Loss::BinaryNotYet],
    );
    // SQL Server: a signed byte does not fit its tinyint; JSON is text.
    check("sqlserver", int(1, false), "smallint", &[]);
    check("sqlserver", int(1, true), "tinyint", &[]);
    check("sqlserver", Portable::TimestampTz, "datetimeoffset(7)", &[]);
    check("sqlserver", Portable::Uuid, "uniqueidentifier", &[]);
    check(
        "sqlserver",
        Portable::Json,
        "nvarchar(max)",
        &[Loss::JsonAsText],
    );
    check(
        "sqlserver",
        Portable::Text { length: Some(80) },
        "nvarchar(80)",
        &[],
    );
    // SQLite's decimals are its own numbers.
    check(
        "sqlite",
        dec(12, 2),
        "DECIMAL(12,2)",
        &[Loss::DecimalApproximate],
    );
    check("sqlite", Portable::Uuid, "TEXT", &[Loss::UuidAsText]);
    // DuckDB has nearly everything, binary included.
    check("duckdb", int(8, true), "UBIGINT", &[]);
    check("duckdb", Portable::Binary { length: None }, "BLOB", &[]);
    check("duckdb", Portable::Uuid, "UUID", &[]);
    // ClickHouse: nullable wraps, a text's length goes, time is text.
    assert_eq!(
        target_type("clickhouse", int(4, false), true).sql_type,
        "Nullable(Int32)"
    );
    check("clickhouse", int(8, true), "UInt64", &[]);
    check(
        "clickhouse",
        Portable::Text { length: Some(80) },
        "String",
        &[Loss::LengthDropped],
    );
    check("clickhouse", Portable::Time, "String", &[Loss::TimeAsText]);
    check(
        "clickhouse",
        Portable::TimestampTz,
        "DateTime64(6)",
        &[Loss::ZoneToUtc],
    );
}

/// Without bytes in `insert_rows`, binary columns go only where the target reads hex text as
/// bytes (ADR-069's point 8); with them (SDB-464), to every target, as bytes, losing nothing.
#[test]
fn binary_is_blocked_only_where_it_would_be_stored_as_text() {
    for target in TARGETS {
        let column = SourceColumn {
            name: "payload".into(),
            native_type: "bytea".into(),
            logical: L::Bytes,
            nullable: true,
        };
        let plan = plan_column("postgres", &column, target, false);
        assert_eq!(
            plan.blocked(),
            !matches!(target, "postgres" | "duckdb"),
            "{target}"
        );
        let plan = plan_column("postgres", &column, target, true);
        assert!(!plan.blocked(), "{target}");
        assert!(plan.target.losses.is_empty(), "{target}: {plan:?}");
        // The type a new table gets for it stores the bytes as they are.
        assert!(
            takes_bytes(target, &plan.target.sql_type),
            "{target}: {}",
            plan.target.sql_type
        );
    }
}

/// Which target columns a binary column's bytes go into as they are (SDB-464).
#[test]
fn bytes_go_as_they_are_only_into_binary_columns() {
    for (engine, native) in [
        ("postgres", "bytea"),
        ("mysql", "varbinary(16)"),
        ("mysql", "LONGBLOB"),
        ("mysql", "binary(4)"),
        ("sqlserver", "varbinary(max)"),
        ("sqlserver", "image"),
        ("sqlite", "BLOB"),
        ("sqlite", ""),
        ("duckdb", "BLOB"),
        ("clickhouse", "String"),
        ("clickhouse", "Nullable(String)"),
        ("clickhouse", "FixedString(16)"),
    ] {
        assert!(takes_bytes(engine, native), "{engine} {native}");
    }
    for (engine, native) in [
        ("postgres", "text"),
        ("mysql", "varchar(10)"),
        ("mysql", "text"),
        ("sqlserver", "nvarchar(max)"),
        ("sqlite", "TEXT"),
        ("duckdb", "VARCHAR"),
        ("clickhouse", "UUID"),
        ("oracle", "BLOB"),
    ] {
        assert!(!takes_bytes(engine, native), "{engine} {native}");
    }
}

/// Nothing a common column becomes is blocked but binary: every other kind reaches every target.
#[test]
fn only_binary_ever_blocks() {
    for target in TARGETS {
        for kind in kinds() {
            let blocked = target_type(target, kind, true)
                .losses
                .iter()
                .any(Loss::blocks);
            if blocked {
                assert!(matches!(kind, Portable::Binary { .. }), "{target} {kind:?}");
            }
        }
    }
}
