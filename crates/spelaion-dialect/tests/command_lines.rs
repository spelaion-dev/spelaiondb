//! Client command lines (ADR-078, SDB-466): every SQL dialect's scanner reads a line that starts
//! with `@` and a letter, where a statement could begin, as a statement of its own that its
//! dialect refuses, and reads nothing else that way. Languages that are not SQL (ADR-056) have
//! none.

use spelaion_dialect::{client_command, for_engine, refuse_client_command};
use spelaion_driver_api::Dialect;

/// Every engine whose dialect is SQL, by the names the configuration uses.
const SQL_ENGINES: &[&str] = &[
    "postgres",
    "mysql",
    "mariadb",
    "sqlite",
    "libsql",
    "sqlserver",
    "duckdb",
    "clickhouse",
    "trino",
    "presto",
    "oracle",
    "databend",
    "spanner",
    "hive",
    "spark",
    "kyuubi",
    "impala",
    "flightsql",
    "dremio",
    "gizmosql",
];

fn dialect(engine: &str) -> std::sync::Arc<dyn Dialect> {
    for_engine(engine).unwrap_or_else(|| panic!("{engine} has no dialect"))
}

fn statements<'a>(d: &dyn Dialect, script: &'a str) -> Vec<&'a str> {
    d.split_statements(script)
        .iter()
        .map(|s| s.text(script))
        .collect()
}

#[test]
fn a_command_line_is_a_statement_of_its_own_that_is_refused() {
    for &engine in SQL_ENGINES {
        let d = dialect(engine);
        for (script, expected) in [
            ("@set x = 1\nselect 1", vec!["@set x = 1", "select 1"]),
            (
                "select 1;\n  @echo it's done\nselect 2",
                vec!["select 1", "@echo it's done", "select 2"],
            ),
            // After comments and blank lines, where a statement could begin.
            (
                "-- first\n\n@include other.sql\r\nselect 3",
                vec!["@include other.sql", "select 3"],
            ),
            ("@unset x;\nselect 4", vec!["@unset x;", "select 4"]),
            (
                "@Export {\"format\": \"csv\"}",
                vec!["@Export {\"format\": \"csv\"}"],
            ),
        ] {
            let got = statements(d.as_ref(), script);
            assert_eq!(got, expected, "{engine}: {script:?}");
            let command = got.iter().find(|s| s.starts_with('@')).unwrap();
            assert!(
                d.refuse_statement(command).is_some(),
                "{engine}: {command:?} was not refused"
            );
            for statement in got.iter().filter(|s| !s.starts_with('@')) {
                assert!(
                    d.refuse_statement(statement).is_none(),
                    "{engine}: {statement:?} refused"
                );
            }
        }
    }
}

#[test]
fn an_at_sign_anywhere_else_is_what_it_was() {
    for &engine in SQL_ENGINES {
        let d = dialect(engine);
        for script in [
            // Inside a string, a quoted name and comments.
            "select 'a\n@set x = 1'",
            "select \"a\n@set x\"",
            "/* a\n@set x = 1\n*/ select 1",
            // In the middle of a statement (MySQL's `@var :=`, T-SQL's `@var =`).
            "select\n@set := 1",
            "select 1,\n  @echo",
            // Not a letter after the `@`: Spanner's statement hint stays SQL.
            "@{FORCE_INDEX=_BASE_TABLE} select 1",
            "@@version",
            // Something before it on its line.
            "x @set",
        ] {
            let got = statements(d.as_ref(), script);
            assert_eq!(got.len(), 1, "{engine}: {script:?} split as {got:?}");
            assert!(
                d.refuse_statement(got[0]).is_none(),
                "{engine}: {script:?} refused"
            );
        }
    }
    // A body quoted by a dialect's own markers.
    for (engine, script) in [
        ("postgres", "select $t$\n@set x = 1\n$t$"),
        ("duckdb", "select $$\n@set x = 1\n$$"),
        ("clickhouse", "select $t$\n@set x = 1\n$t$"),
        ("databend", "select $$\n@set x = 1\n$$"),
        ("spanner", "select \"\"\"\n@set x = 1\n\"\"\""),
    ] {
        let d = dialect(engine);
        assert_eq!(
            statements(d.as_ref(), script).len(),
            1,
            "{engine}: {script:?}"
        );
    }
}

#[test]
fn a_quote_in_a_command_line_hides_nothing_after_it() {
    for &engine in SQL_ENGINES {
        let d = dialect(engine);
        let script = "@set name = O'Brien\nselect :p from t";
        let at = script.find(":p").unwrap();
        assert!(
            d.code_ranges(script).iter().any(|r| r.contains(&at)),
            "{engine}: the placeholder after a command line was hidden"
        );
        assert_eq!(
            statements(d.as_ref(), script),
            ["@set name = O'Brien", "select :p from t"],
            "{engine}"
        );
    }
}

#[test]
fn languages_that_are_not_sql_have_no_command_lines() {
    for engine in ["cassandra", "tdengine"] {
        let d = dialect(engine);
        let script = "@set x = 1\nselect 1";
        assert_eq!(statements(d.as_ref(), script).len(), 1, "{engine}");
    }
}

#[test]
fn each_refusal_says_what_the_line_is() {
    assert_eq!(client_command("@SET x = 1").as_deref(), Some("set"));
    assert_eq!(client_command("  @echo_2 hi").as_deref(), Some("echo_2"));
    assert_eq!(client_command("@{hint}"), None);
    assert_eq!(client_command("select 1"), None);
    let export = refuse_client_command("@export {}").unwrap().message;
    assert!(
        export.contains("run the query, then use Export"),
        "{export}"
    );
    for known in ["@set x = 1", "@unset x", "@echo hi", "@include a.sql"] {
        let message = refuse_client_command(known).unwrap().message;
        assert!(message.contains("nothing runs it here"), "{message}");
    }
    let unknown = refuse_client_command("@spool out.txt").unwrap().message;
    assert!(
        unknown.contains("@spool is not a command") && unknown.contains("never sent"),
        "{unknown}"
    );
    assert!(refuse_client_command("select 1").is_none());
}
