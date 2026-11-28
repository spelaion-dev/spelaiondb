//! `${name}` variables against the guard (SDB-383), on every dialect the core has. A value
//! is written into the statement as it is, so these try to use one to get a statement past
//! the guard: a value that ends its statement and starts another, one that opens a comment or
//! a string over the rest of the statement, and a `DROP` or a `DELETE` without `WHERE` made
//! out of a value. Nothing external: only the dialects and the guard.

use std::collections::HashMap;
use std::sync::Arc;

use spelaion_config::Safeguards;
use spelaion_core::{Risk, VariableError, script_risks, script_variables, substitute_variables};
use spelaion_driver_api::Dialect;

/// Every engine name `spelaion_dialect::for_engine` answers to.
const ENGINES: &[&str] = &[
    "postgres",
    "mysql",
    "mariadb",
    "sqlite",
    "sqlserver",
    "duckdb",
    "mongodb",
    "clickhouse",
    "redis",
    "trino",
    "presto",
    "oracle",
    "neo4j",
    "databend",
    "spanner",
    "libsql",
    "cassandra",
    "tdengine",
    "flightsql",
    "dremio",
    "gizmosql",
    "hive",
    "spark",
    "kyuubi",
    "impala",
    "elasticsearch",
    "opensearch",
    "solr",
    "meilisearch",
];

/// The engines whose language is SQL written as text with `;` between statements, which the
/// guard parses: the ones the `; DROP …` attempts are written for.
const SQL_ENGINES: &[&str] = &[
    "postgres",
    "mysql",
    "mariadb",
    "sqlite",
    "sqlserver",
    "duckdb",
    "clickhouse",
    "trino",
    "presto",
    "oracle",
    "databend",
    "spanner",
    "libsql",
    "flightsql",
    "dremio",
    "gizmosql",
    "hive",
    "spark",
    "kyuubi",
    "impala",
];

fn dialect(engine: &str) -> Arc<dyn Dialect> {
    spelaion_dialect::for_engine(engine).unwrap_or_else(|| panic!("no dialect for {engine}"))
}

fn values(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// What the guard asks about a script, as the run would be asked.
fn risks(dialect: &dyn Dialect, script: &str) -> Vec<Risk> {
    script_risks(dialect, script, &Safeguards::default())
        .into_iter()
        .map(|risky| risky.risk)
        .collect()
}

#[test]
fn every_dialect_is_covered() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in ENGINES {
        dialect(engine);
    }
    for engine in SQL_ENGINES {
        assert!(ENGINES.contains(engine), "{engine}");
    }
}

/// On every dialect, a script comes out with as many statements as it went in with, or it
/// is refused: whatever the value holds.
#[test]
fn no_value_changes_how_many_statements_a_script_has() {
    let _turn = crate::serial::file_turn(module_path!());
    let attempts = [
        "1; DROP TABLE users",
        "1;\nDROP TABLE users",
        "1; DROP TABLE users; --",
        "1; DROP TABLE users /*",
        "1 -- ",
        "1 /* ",
        "1 # ",
        "'",
        "\"",
        "`",
        "1'; DROP TABLE users; --",
        "x\nGO\nDROP TABLE users",
        "x\n/\nDROP TABLE users",
        "1)\ndb.users.drop(",
        "x\nFLUSHALL",
        "$$",
        "q'[",
        "",
        ";",
        "\n",
    ];
    let scripts = [
        "select * from t where id = ${v} and tenant = 1",
        "select * from t where id = ${v}",
        "select 1;\nselect * from t where id = ${v} and tenant = 1;\nselect 2",
        "${v}",
        "db.users.find({id: ${v}})",
        "MATCH (n) WHERE n.id = ${v} RETURN n",
        "GET ${v}",
    ];
    for engine in ENGINES {
        let dialect = dialect(engine);
        for script in scripts {
            let before = dialect.split_statements(script).len();
            for attempt in attempts {
                match substitute_variables(dialect.as_ref(), script, &values(&[("v", attempt)])) {
                    Ok(written) => {
                        assert_eq!(
                            dialect.split_statements(&written).len(),
                            before,
                            "{engine}: {script:?} with {attempt:?} became {written:?}"
                        );
                        // And each statement on its own is still one statement.
                        for span in dialect.split_statements(&written) {
                            let statement = &written[span.start..span.end];
                            assert_eq!(
                                dialect.split_statements(statement).len(),
                                1,
                                "{engine}: {statement:?}"
                            );
                        }
                    }
                    Err(
                        VariableError::SplitsStatement(_)
                        | VariableError::OpensText(_)
                        | VariableError::InvalidValue(_),
                    ) => {}
                    Err(VariableError::Missing(name)) => {
                        panic!("{engine}: {script:?} has no value for {name}")
                    }
                    // Only the editor's run resolves commands; substituting never reads one.
                    Err(VariableError::Command(reason)) => {
                        panic!("{engine}: {script:?} read as a command: {reason}")
                    }
                    // Nor does it include a file.
                    Err(e @ (VariableError::Include(_) | VariableError::InFile { .. })) => {
                        panic!("{engine}: {script:?} included a file: {e}")
                    }
                }
            }
        }
    }
}

/// `; DROP …` in a value never runs as a second statement: the substitution is refused on
/// every SQL dialect, before the guard is even asked.
#[test]
fn a_value_holding_a_second_statement_is_refused_on_every_sql_dialect() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in SQL_ENGINES {
        let dialect = dialect(engine);
        let script = "select * from orders where customer = ${customer}";
        assert_eq!(
            script_variables(dialect.as_ref(), script),
            ["customer"],
            "{engine}"
        );
        for attempt in [
            "1; DROP TABLE orders",
            "1; DROP TABLE orders --",
            "1;DELETE FROM orders",
            "1; TRUNCATE TABLE orders;",
        ] {
            let refused =
                substitute_variables(dialect.as_ref(), script, &values(&[("customer", attempt)]));
            assert!(
                matches!(refused, Err(VariableError::SplitsStatement(_))),
                "{engine}: {attempt:?} gave {refused:?}"
            );
        }
    }
}

/// A value cannot comment out, or swallow in a string, what follows it in its statement: the
/// `WHERE` the guard would read stays where the user wrote it.
#[test]
fn a_value_cannot_hide_the_rest_of_its_statement_on_any_sql_dialect() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in SQL_ENGINES {
        let dialect = dialect(engine);
        let script = "delete from orders where customer = ${customer} and status = 'draft'";
        for attempt in ["1 --", "1 -- ", "1 /*", "'", "'x"] {
            let refused =
                substitute_variables(dialect.as_ref(), script, &values(&[("customer", attempt)]));
            assert_eq!(
                refused,
                Err(VariableError::OpensText("${customer}".into())),
                "{engine}: {attempt:?}"
            );
        }
        // A comment a value opens and closes is its own, and the statement is read whole.
        let written = substitute_variables(
            dialect.as_ref(),
            script,
            &values(&[("customer", "1 /* was 2 */")]),
        )
        .unwrap_or_else(|e| panic!("{engine}: {e}"));
        assert_eq!(
            written,
            "delete from orders where customer = 1 /* was 2 */ and status = 'draft'"
        );
        assert!(risks(dialect.as_ref(), &written).is_empty(), "{engine}");
    }
}

/// What a value makes of a statement, the guard reads: it is given the text that runs. A
/// `DROP` whose target is a variable, and a `DELETE` or `UPDATE` whose `WHERE` is a variable
/// left empty or commented out, ask like the same statement typed by hand.
#[test]
fn the_guard_reads_the_statement_with_its_values_on_every_sql_dialect() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in SQL_ENGINES {
        let dialect = dialect(engine);
        let check = |script: &str, pairs: &[(&str, &str)], expected: &[Risk]| {
            let written = substitute_variables(dialect.as_ref(), script, &values(pairs))
                .unwrap_or_else(|e| panic!("{engine}: {script:?}: {e}"));
            assert_eq!(
                risks(dialect.as_ref(), &written),
                expected,
                "{engine}: {written:?}"
            );
            // The same as typing it.
            assert_eq!(risks(dialect.as_ref(), &written), expected);
        };
        // Before the values are in, the guard would not know: it must never be asked then.
        check("drop table ${table}", &[("table", "orders")], &[Risk::Drop]);
        check("${verb} table orders", &[("verb", "drop")], &[Risk::Drop]);
        check(
            "delete from orders ${filter}",
            &[("filter", "")],
            &[Risk::DeleteAll],
        );
        check(
            "delete from orders ${filter}",
            &[("filter", "/* where id = 1 */")],
            &[Risk::DeleteAll],
        );
        check(
            "update orders set status = 'x' ${filter}",
            &[("filter", "-- where id = 1")],
            &[Risk::UpdateAll],
        );
        check(
            "delete from orders ${filter}",
            &[("filter", "where id = 1")],
            &[],
        );
        // A whole statement in a variable is read as that statement.
        check("${all}", &[("all", "drop table orders")], &[Risk::Drop]);
        // A statement that is safe as written, with a value that makes it one that is not.
        check(
            "select * from orders; ${more}",
            &[("more", "delete from orders")],
            &[Risk::DeleteAll],
        );
    }
}

/// A variable is only what the user set: never the environment, whatever it is called.
#[test]
fn a_name_is_never_read_from_the_environment() {
    let _turn = crate::serial::file_turn(module_path!());
    // SAFETY-free: the variable is read, not set; PATH exists on every machine that runs this.
    let path = std::env::var("PATH").unwrap_or_default();
    assert!(!path.is_empty());
    for engine in SQL_ENGINES {
        let dialect = dialect(engine);
        assert_eq!(
            substitute_variables(dialect.as_ref(), "select ${PATH}", &values(&[])),
            Err(VariableError::Missing("PATH".into())),
            "{engine}"
        );
        // The connection files' `${env:…}` and `${user.…}` mean nothing in a script: they are
        // not variables, and stay as written for the server to refuse.
        for script in [
            "select ${env:PATH}",
            "select ${user.name}",
            "select ${file:/etc/passwd}",
        ] {
            assert!(
                script_variables(dialect.as_ref(), script).is_empty(),
                "{engine}"
            );
            assert_eq!(
                substitute_variables(dialect.as_ref(), script, &values(&[("PATH", "x")]))
                    .as_deref(),
                Ok(script),
                "{engine}"
            );
        }
    }
}

/// In a string, a quoted name or a comment, `${…}` is the user's text on every dialect.
#[test]
fn variables_in_strings_names_and_comments_are_never_touched() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in SQL_ENGINES {
        let dialect = dialect(engine);
        let script = "select '${v}' as a -- ${v}\n, 1 /* ${v} */ from t";
        assert!(
            script_variables(dialect.as_ref(), script).is_empty(),
            "{engine}"
        );
        assert_eq!(
            substitute_variables(
                dialect.as_ref(),
                script,
                &values(&[("v", "; drop table t")])
            )
            .as_deref(),
            Ok(script),
            "{engine}"
        );
    }
}

/// A language the core has no code ranges for (one command or one request per statement) has
/// no variables: nothing is written into it.
#[test]
fn a_language_without_code_ranges_has_no_variables() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in [
        "redis",
        "elasticsearch",
        "opensearch",
        "solr",
        "meilisearch",
    ] {
        let dialect = dialect(engine);
        let script = "GET ${v}";
        assert!(
            script_variables(dialect.as_ref(), script).is_empty(),
            "{engine}"
        );
        assert_eq!(
            substitute_variables(dialect.as_ref(), script, &values(&[("v", "x\nFLUSHALL")]))
                .as_deref(),
            Ok(script),
            "{engine}"
        );
    }
}

// ------------------------------------------------- client commands in the editor's runs (ADR-078)

/// What `@set` attempts to smuggle: a statement separator, an opening quote or comment, a
/// command line of its own, a `DROP` after a `;`.
const SET_ATTEMPTS: &[&str] = &[
    "t; drop table u",
    "t -- rest",
    "t /* rest",
    "'open",
    "\"open",
    "`open",
    "[open",
    "$$ open",
    "t\n@set y = 1",
    "1); delete from u; --",
];

#[test]
fn a_set_value_never_adds_a_statement_nor_hides_one_from_the_guard() {
    let _turn = crate::serial::file_turn(module_path!());
    for &engine in SQL_ENGINES {
        let d = dialect(engine);
        for value in SET_ATTEMPTS {
            let script = format!("@set v = {value}\nselect a from ${{v}};\nselect 2");
            match spelaion_core::resolve_script(d.as_ref(), &script, &values(&[]), None) {
                Ok(resolved) => {
                    let statements = d.split_statements(&resolved.text);
                    assert_eq!(
                        statements.len(),
                        2,
                        "{engine}: {script:?} -> {:?}",
                        resolved.text
                    );
                    // On the standard reading, a value with a marker another reading reads
                    // otherwise (`$$`) asks about that reading (ADR-079, point 6): more asked,
                    // never less. Nothing else asks.
                    assert!(
                        risks(d.as_ref(), &resolved.text)
                            .iter()
                            .all(|risk| d.standard_reading() && *risk == Risk::OtherReading),
                        "{engine}: {script:?} made a statement the guard asks about"
                    );
                }
                Err(VariableError::SplitsStatement(_) | VariableError::OpensText(_)) => {}
                Err(other) => panic!("{engine}: {script:?}: {other}"),
            }
        }
    }
}

#[test]
fn set_and_unset_change_values_from_their_line_on_for_this_run_only() {
    let _turn = crate::serial::file_turn(module_path!());
    for &engine in SQL_ENGINES {
        let d = dialect(engine);
        let tab = values(&[("t", "orders")]);
        let before = tab.clone();
        let script = "select * from ${t};\n@set t = customers\nselect * from ${t};\n@echo done\n@unset t\nselect 1";
        let resolved = spelaion_core::resolve_script(d.as_ref(), script, &tab, None).unwrap();
        let statements: Vec<&str> = d
            .split_statements(&resolved.text)
            .iter()
            .map(|s| s.text(&resolved.text))
            .collect();
        assert_eq!(
            statements,
            [
                "select * from orders",
                "select * from customers",
                "select 1"
            ],
            "{engine}"
        );
        assert_eq!(
            resolved.echoes,
            [spelaion_core::ScriptEcho {
                before: 2,
                line: spelaion_core::EchoLine::Echo("done".into())
            }],
            "{engine}"
        );
        // The tab's values are what they were: a script never changes them.
        assert_eq!(tab, before);
        // After `@unset`, a `${t}` has no value: refused before anything runs, never asked for.
        let unset = "@unset t\nselect * from ${t}";
        assert_eq!(
            spelaion_core::resolve_script(d.as_ref(), unset, &tab, None),
            Err(VariableError::Missing("t".into())),
            "{engine}"
        );
        assert!(spelaion_core::missing_variables(d.as_ref(), unset, &tab, None).is_empty());
        assert!(
            spelaion_core::missing_variables(
                d.as_ref(),
                "@set u = 1\nselect ${u}, ${w}",
                &tab,
                None
            ) == ["w"]
        );
        // A command written otherwise than it reads is refused, saying how it is written.
        for bad in ["@set = 1", "@set 1x = 2", "@unset", "@unset a b"] {
            assert!(
                matches!(
                    spelaion_core::resolve_script(d.as_ref(), bad, &tab, None),
                    Err(VariableError::Command(_))
                ),
                "{engine}: {bad:?}"
            );
        }
        // `@include`, `@export` and an unknown `@word` are left for the session to refuse.
        let left = "@export {}\nselect 1";
        let resolved = spelaion_core::resolve_script(d.as_ref(), left, &tab, None).unwrap();
        assert!(resolved.text.contains("@export"), "{engine}");
    }
}

#[test]
fn a_read_only_connection_runs_commands_and_reads_and_refuses_a_write() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in [
        "postgres",
        "mysql",
        "sqlite",
        "sqlserver",
        "duckdb",
        "oracle",
    ] {
        let d = dialect(engine);
        let reads = "@set t = items\n@echo reading\nselect * from ${t};\nselect 1";
        let resolved =
            spelaion_core::resolve_script(d.as_ref(), reads, &values(&[]), None).unwrap();
        assert!(
            spelaion_core::check_read_only(d.as_ref(), &resolved.text).is_ok(),
            "{engine}: {:?}",
            resolved.text
        );
        let write = "@set t = items\nselect * from ${t};\ndelete from ${t} where a = 1";
        let resolved =
            spelaion_core::resolve_script(d.as_ref(), write, &values(&[]), None).unwrap();
        assert!(
            spelaion_core::check_read_only(d.as_ref(), &resolved.text).is_err(),
            "{engine}: a write passed"
        );
    }
}
