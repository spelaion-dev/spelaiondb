//! Property tests of what in the core reads text no one wrote by hand (SDB-156): the `:name`
//! placeholder scanner, the read-only classifiers and the editor's guard, the import reader and
//! the `.sql` file reader, and the SQLite splitter against SQLite itself.
//!
//! In CI the cases are bounded and the seed is fixed, so a run is the same every time and a
//! failure prints its smallest input. `SPELAION_FUZZ_CASES=<n>` runs `n` cases per property
//! with a random seed (`PROPTEST_RNG_SEED` fixes it), for long runs on a machine or on a
//! schedule, never in a pull request's job.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, RngSeed};
use spelaion_config::Safeguards;
use spelaion_core::{
    ImportFormat, ImportSource, ParseOptions, Risk, SourceEncoding, bind_params, check_read_only,
    check_script, classify_read_only, classify_read_only_connection, guess_options, preview,
    read_sql_file, script_params, script_risks,
};
use spelaion_dialect::{MysqlDialect, for_engine};
use spelaion_driver_api::{Dialect, StatementSpan};

fn config(cases: u32) -> Config {
    let fuzz = std::env::var("SPELAION_FUZZ_CASES")
        .ok()
        .and_then(|v| v.parse().ok());
    match fuzz {
        Some(cases) => Config {
            cases,
            failure_persistence: None,
            ..Config::default()
        },
        None => Config {
            cases,
            rng_seed: RngSeed::Fixed(0x5DB_0156),
            failure_persistence: None,
            ..Config::default()
        },
    }
}

fn dialect(engine: &str) -> Arc<dyn Dialect> {
    for_engine(engine).unwrap_or_else(|| panic!("{engine} has no dialect"))
}

type S = BoxedStrategy<String>;

fn text_of(chars: &str, max: usize) -> S {
    let chars: Vec<char> = chars.chars().collect();
    prop::collection::vec(prop::sample::select(chars), 0..max)
        .prop_map(|v| v.into_iter().collect())
        .boxed()
}

/// A temporary file for one case: unique to the process and the case.
fn temp_file(name: &str, bytes: &[u8]) -> PathBuf {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "spelaion-properties-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn remove_temp(path: &std::path::Path) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

// ------------------------------------------------------------------------------ placeholders

/// The SQL dialects the placeholder scanner reads scripts for, with a label; MySQL under each
/// `sql_mode` that reads text otherwise.
fn sql_dialects() -> Vec<(String, Arc<dyn Dialect>)> {
    let mut all: Vec<(String, Arc<dyn Dialect>)> = [
        "postgres",
        "mysql",
        "sqlite",
        "sqlserver",
        "duckdb",
        "clickhouse",
        "trino",
        "oracle",
        "databend",
        "spanner",
        "cassandra",
        "tdengine",
        "flightsql",
        "hive",
    ]
    .iter()
    .map(|e| (e.to_string(), dialect(e)))
    .collect();
    for mode in ["NO_BACKSLASH_ESCAPES", "ANSI_QUOTES"] {
        all.push((
            format!("mysql[{mode}]"),
            MysqlDialect.with_sql_mode(mode).unwrap(),
        ));
    }
    all
}

/// What holds `:p` without being a placeholder, on every dialect: strings, quoted names and
/// comments, casts, assignments, labels and a colon joined to a word.
fn common_decoys() -> Vec<S> {
    let body = || text_of("ab ;:=-.@é日", 6);
    vec![
        body().prop_map(|b| format!("'{b} :p {b}'")).boxed(),
        body().prop_map(|b| format!("\"{b}:p\"")).boxed(),
        body().prop_map(|b| format!("-- {b} :p\n")).boxed(),
        body().prop_map(|b| format!("/* {b} :p */")).boxed(),
        Just("x::int".to_string()).boxed(),
        Just("@v:=1".to_string()).boxed(),
        Just("a:b".to_string()).boxed(),
        Just("t.c:p".to_string()).boxed(),
    ]
}

/// What each dialect adds: the non-code it reads that a simpler scanner would not (SDB-135).
fn extra_decoys(label: &str) -> Vec<S> {
    let mut extra: Vec<S> = Vec::new();
    let fixed = |v: &[&str]| -> Vec<S> { v.iter().map(|s| Just(s.to_string()).boxed()).collect() };
    match label {
        "postgres" | "duckdb" => {
            extra.extend(fixed(&[
                "E'it\\'s :p'",
                "$$ it's :p $$",
                "$t$ :p $$ :q $t$",
                "/* a /* b */ :p */",
            ]));
            if label == "duckdb" {
                extra.extend(fixed(&["{'k':v}", "{'a': 1, 'b':c}"]));
            }
        }
        "mysql" => extra.extend(fixed(&["'it\\'s :p'", "`a:p`", "# :p\n", "/*!40101 :p */"])),
        "mysql[NO_BACKSLASH_ESCAPES]" => extra.extend(fixed(&["'C:\\' ", "`:p`"])),
        "mysql[ANSI_QUOTES]" => extra.extend(fixed(&["\"a\\\" ", "`:p`"])),
        "sqlserver" => extra.extend(fixed(&["[a]]:p]", "/* /* */ :p */", "[a]:b", "N':p'"])),
        "sqlite" => extra.extend(fixed(&["[:p]", "`:p`", "[a]:b"])),
        "oracle" => extra.extend(fixed(&["q'[it's :p]'", "nq'{:p}'"])),
        "clickhouse" => extra.extend(fixed(&["'it\\'s :p'", "`:p`", "# :p\n", "$h$ :p $h$"])),
        "databend" => extra.extend(fixed(&["'it\\'s :p'", "`:p`", "$$ :p $$"])),
        "spanner" => extra.extend(fixed(&["'it\\'s :p'", "# :p\n", "''' :p\n'''", "`:p`"])),
        "cassandra" => extra.extend(fixed(&["// :p\n", "$$ it's :p $$"])),
        "tdengine" | "hive" => extra.extend(fixed(&["'it\\'s :p'", "`:p`"])),
        _ => {}
    }
    extra
}

fn decoy(label: &str) -> S {
    Union::new(common_decoys().into_iter().chain(extra_decoys(label))).boxed()
}

/// A part of a script with placeholders.
#[derive(Debug, Clone)]
enum Part {
    Text(String),
    Param(String),
}

fn parts(label: &str) -> BoxedStrategy<Vec<Part>> {
    let part = prop_oneof![
        3 => decoy(label).prop_map(Part::Text),
        2 => prop::sample::select(vec!["select", "x", "=", "(", ")", ",", "where", "1", "and"])
            .prop_map(|s| Part::Text(s.to_string())),
        2 => "[a-z_][a-z0-9_]{0,5}".prop_map(Part::Param),
    ];
    prop::collection::vec(part, 0..12).boxed()
}

/// The script, its placeholders in order of first appearance, and the script with each
/// placeholder written as `7`.
fn assemble(parts: &[Part]) -> (String, Vec<String>, String) {
    let mut script = String::from("select");
    let mut bound = String::from("select");
    let mut names: Vec<String> = Vec::new();
    for part in parts {
        script.push(' ');
        bound.push(' ');
        match part {
            Part::Text(text) => {
                script.push_str(text);
                bound.push_str(text);
            }
            Part::Param(name) => {
                script.push(':');
                script.push_str(name);
                bound.push('7');
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
        }
    }
    (script, names, bound)
}

fn labelled_parts() -> BoxedStrategy<(String, Vec<Part>)> {
    Union::new(sql_dialects().into_iter().map(|(label, _)| {
        let l = label.clone();
        parts(&label).prop_map(move |p| (l.clone(), p)).boxed()
    }))
    .boxed()
}

fn dialect_of(label: &str) -> Arc<dyn Dialect> {
    match label.strip_prefix("mysql[") {
        Some(mode) => MysqlDialect
            .with_sql_mode(mode.trim_end_matches(']'))
            .unwrap(),
        None => dialect(label),
    }
}

proptest! {
    #![proptest_config(config(512))]

    /// Any text: the scanner and the binder never panic, and binding every placeholder
    /// leaves none.
    #[test]
    fn any_text_has_placeholders_that_bind(script in any::<String>(), soup in text_of(":'\"`[]$-/*#\\ \nabxé=(),;", 40)) {
        let _turn = crate::serial::file_turn(module_path!());
        for (label, dialect) in sql_dialects() {
            for script in [&script, &soup] {
                let names = script_params(dialect.as_ref(), script);
                let values: HashMap<String, String> =
                    names.iter().map(|n| (n.clone(), "1".to_string())).collect();
                let bound = bind_params(dialect.as_ref(), script, &values);
                prop_assert!(bound.is_ok(), "{}: {:?} does not bind", label, script);
                if names.is_empty() {
                    prop_assert_eq!(bound.unwrap(), script.to_string(), "{}", label);
                }
            }
        }
    }

    /// Placeholders among strings, quoted names, comments, casts and bodies that hold `:p`:
    /// exactly the placeholders are found, in order, and binding replaces them and nothing
    /// else (SDB-135).
    #[test]
    fn only_placeholders_in_code_are_found_and_bound((label, parts) in labelled_parts()) {
        let _turn = crate::serial::file_turn(module_path!());
        let dialect = dialect_of(&label);
        let (script, names, bound) = assemble(&parts);
        prop_assert_eq!(script_params(dialect.as_ref(), &script), names.clone(), "{}: {:?}", label, script);
        let values: HashMap<String, String> = names.iter().map(|n| (n.clone(), "7".into())).collect();
        prop_assert_eq!(bind_params(dialect.as_ref(), &script, &values).unwrap(), bound, "{}", label);
    }
}

proptest! {
    #![proptest_config(config(256))]

    /// Whatever the user types as a value stays one literal, on every dialect: the script
    /// splits into as many statements as before and has no placeholder left (SDB-435).
    #[test]
    fn a_value_stays_inside_its_literal(
        label in prop::sample::select(sql_dialects().into_iter().map(|(l, _)| l).collect::<Vec<_>>()),
        parts in parts("standard"),
        value in prop_oneof![
            any::<String>(),
            text_of("'\"\\;:-/*` xé", 12),
            text_of("'\"\\;:-/*` xé", 12).prop_map(|v| v + "\\"),
        ],
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let dialect = dialect_of(&label);
        let (script, names, _) = assemble(&parts);
        let script = format!("{script};\nselect 2");
        let values: HashMap<String, String> = names.iter().map(|n| (n.clone(), value.clone())).collect();
        let bound = bind_params(dialect.as_ref(), &script, &values).unwrap();
        prop_assert_eq!(
            dialect.split_statements(&bound).len(),
            dialect.split_statements(&script).len(),
            "{}: {:?}", label, bound
        );
        prop_assert!(script_params(dialect.as_ref(), &bound).is_empty(), "{}: {:?}", label, bound);
    }
}

/// Found by `a_value_stays_inside_its_literal` when every dialect was tried (SDB-435). Where a
/// backslash escapes a quote in a string (ClickHouse, Databend, Spanner, HiveQL, TDengine),
/// `bind_params` wrote a value as `'…'` with doubled quotes only: a value ending in `\` ended
/// its literal early and the rest of the script was read inside a string.
#[test]
fn a_value_with_a_backslash_stays_inside_its_literal_everywhere() {
    let _turn = crate::serial::file_turn(module_path!());
    let mut broken = Vec::new();
    for engine in [
        "clickhouse",
        "databend",
        "spanner",
        "hive",
        "spark",
        "kyuubi",
        "impala",
        "tdengine",
    ] {
        let dialect = dialect(engine);
        let script = "select :a;\nselect 2";
        for value in ["C:\\", "x\\'", "\\\\'; select 3; --\\", "it's"] {
            let values = HashMap::from([("a".to_string(), value.to_string())]);
            let bound = bind_params(dialect.as_ref(), script, &values).unwrap();
            if dialect.split_statements(&bound).len() != 2 {
                broken.push(format!("{engine}: {bound:?}"));
            }
        }
    }
    assert!(
        broken.is_empty(),
        "values that leave their literal:\n{}",
        broken.join("\n")
    );
}

// --------------------------------------------------------------------------------- barriers

/// The engines whose statements the core's classifier reads (sqlparser) for reads and the
/// guard checks.
const BARRIER_ENGINES: &[&str] = &[
    "postgres",
    "mysql",
    "mariadb",
    "sqlite",
    "sqlserver",
    "duckdb",
    "clickhouse",
    "oracle",
    "trino",
];

/// A table's name in the forms each engine quotes it.
fn table(engine: &str) -> S {
    let forms: Vec<&'static str> = match engine {
        "mysql" | "mariadb" | "clickhouse" => vec!["t", "`t`", "s.t", "`s`.`t`", "`a b`"],
        "sqlserver" => vec!["t", "[t]", "dbo.t", "[dbo].[t]", "\"t\"", "[a b]"],
        "sqlite" => vec!["t", "\"t\"", "[t]", "`t`", "main.t"],
        _ => vec!["t", "\"t\"", "s.t", "\"s\".\"t\"", "\"a b\""],
    };
    prop::sample::select(forms).prop_map(str::to_string).boxed()
}

/// A statement that changes something, written with `t`.
fn write(engine: &str) -> S {
    let engine = engine.to_string();
    (
        prop::sample::select(vec![
            "delete from {t}",
            "DELETE FROM {t} WHERE a = 1",
            "update {t} set a = 1",
            "UPDATE {t} SET a = 1 WHERE b = 2",
            "insert into {t} values (1)",
            "INSERT INTO {t} SELECT * FROM u",
            "drop table {t}",
            "truncate table {t}",
            "create table {t} (a int)",
            "alter table {t} add b int",
            "grant select on {t} to someone",
            "merge into {t} using u on (1 = 1) when matched then delete",
            "replace into {t} values (1)",
        ]),
        table(&engine),
    )
        .prop_map(|(template, t)| template.replace("{t}", &t))
        .boxed()
}

/// Where a write can hide around a read: behind comments, after a read, in a CTE, under
/// `EXPLAIN ANALYZE`, in MySQL's executable comments, in a transaction.
fn hidden_write(engine: &str) -> S {
    let comment = text_of("ab ;'\"-", 8);
    let mut forms: Vec<S> = vec![
        write(engine),
        (comment.clone(), write(engine))
            .prop_map(|(c, w)| format!("/* {c} */ {w}"))
            .boxed(),
        (comment.clone(), write(engine))
            .prop_map(|(c, w)| format!("-- {c}\n{w}"))
            .boxed(),
        write(engine).prop_map(|w| format!("select 1; {w}")).boxed(),
        write(engine).prop_map(|w| format!("{w}; select 1")).boxed(),
        write(engine)
            .prop_map(|w| format!("with x as (select 1) {w}"))
            .boxed(),
        write(engine)
            .prop_map(|w| format!("with x as ({w} returning *) select * from x"))
            .boxed(),
        write(engine)
            .prop_map(|w| format!("explain analyze {w}"))
            .boxed(),
        write(engine)
            .prop_map(|w| format!("begin; {w}; commit"))
            .boxed(),
    ];
    if matches!(engine, "mysql" | "mariadb") {
        forms.push(write(engine).prop_map(|w| format!("/*! {w} */")).boxed());
        forms.push(
            write(engine)
                .prop_map(|w| format!("select 1 /*!50000 ; {w} */"))
                .boxed(),
        );
        forms.push(
            write(engine)
                .prop_map(|w| format!("/*M!100000 {w} */"))
                .boxed(),
        );
    }
    Union::new(forms).boxed()
}

/// Functions the classifier refuses by name on `engine` (they reach outside the database or
/// run SQL given as text).
fn refused_functions(engine: &str) -> Vec<&'static str> {
    match engine {
        "postgres" => vec![
            "pg_read_file('x')",
            "dblink('c', 'delete from t')",
            "set_config('a', 'b', false)",
            "lo_import('x')",
            "pg_terminate_backend(1)",
            "query_to_xml('delete from t', true, true, '')",
            "pg_ls_dir('.')",
            "crosstab('select 1')",
        ],
        "mysql" | "mariadb" => vec!["load_file('x')", "get_lock('a', 1)", "sys_exec('x')"],
        "sqlite" => vec![
            "load_extension('x')",
            "readfile('x')",
            "writefile('x', 'y')",
        ],
        "sqlserver" => vec![
            "openrowset('SQLNCLI', 'x', 'select 1')",
            "openquery(srv, 'delete from t')",
            "opendatasource('SQLNCLI', 'x')",
        ],
        "duckdb" => vec![
            "read_csv('x')",
            "getenv('HOME')",
            "read_text('x')",
            "glob('*')",
        ],
        "clickhouse" => vec![
            "file('x')",
            "url('http://x')",
            "remote('h', 'd', 't')",
            "s3('x')",
        ],
        "trino" => vec!["query('delete from t')"],
        _ => vec![],
    }
}

/// A read that calls a refused function somewhere: in the select list, a filter, a subquery,
/// a CTE, a `CASE`, another function's argument, an order, a derived table; named in capitals,
/// with its schema or quoted where the engine allows it, with a comment before its `(`.
fn hidden_function(engine: &str) -> Option<S> {
    let functions = refused_functions(engine);
    if functions.is_empty() {
        return None;
    }
    let schema = match engine {
        "postgres" => Some("pg_catalog."),
        "sqlserver" => Some("dbo."),
        _ => None,
    };
    let quote: Option<(char, char)> = match engine {
        "postgres" | "sqlite" | "duckdb" | "trino" => Some(('"', '"')),
        "mysql" | "mariadb" | "clickhouse" => Some(('`', '`')),
        "sqlserver" => Some(('[', ']')),
        _ => None,
    };
    let name_form = (prop::sample::select(functions), 0..5usize).prop_map(move |(call, form)| {
        let (name, args) = call.split_at(call.find('(').unwrap());
        match form {
            1 => format!("{}{args}", name.to_ascii_uppercase()),
            2 => match schema {
                Some(s) => format!("{s}{name}{args}"),
                None => call.to_string(),
            },
            3 => match quote {
                Some((open, close)) => format!("{open}{name}{close}{args}"),
                None => call.to_string(),
            },
            4 => format!("{name} /* c */ {args}"),
            _ => call.to_string(),
        }
    });
    let places = prop::sample::select(vec![
        "select {f}",
        "select a from t where b = {f}",
        "select (select {f})",
        "with x as (select {f} as v) select v from x",
        "select case when 1 = 1 then {f} end",
        "select coalesce(null, {f})",
        "select a from t order by {f}",
        "select * from (select {f} as v) as d",
        "select a from t where exists (select {f})",
        "select * from {f}",
    ]);
    Some(
        (places, name_form)
            .prop_map(|(place, f)| place.replace("{f}", &f))
            .boxed(),
    )
}

fn labelled_hidden() -> BoxedStrategy<(&'static str, String)> {
    Union::new(BARRIER_ENGINES.iter().flat_map(|&engine| {
        let mut both = vec![hidden_write(engine).prop_map(move |s| (engine, s)).boxed()];
        if let Some(f) = hidden_function(engine) {
            both.push(f.prop_map(move |s| (engine, s)).boxed());
        }
        both
    }))
    .boxed()
}

/// A statement that removes or changes every row, or a whole table, and the guard asks about.
fn destructive(engine: &str) -> S {
    (
        prop::sample::select(vec![
            "delete from {t}",
            "update {t} set a = 1",
            "drop table {t}",
            "truncate table {t}",
            "DROP TABLE IF EXISTS {t}",
        ]),
        table(engine),
    )
        .prop_map(|(template, t)| template.replace("{t}", &t))
        .boxed()
}

fn hidden_destructive(engine: &'static str) -> BoxedStrategy<(&'static str, String)> {
    let comment = text_of("ab ;'\"-", 8);
    let mut forms: Vec<S> = vec![
        destructive(engine),
        (comment.clone(), destructive(engine))
            .prop_map(|(c, w)| format!("/* {c} */ {w}"))
            .boxed(),
        (comment, destructive(engine))
            .prop_map(|(c, w)| format!("-- {c}\n{w}"))
            .boxed(),
        destructive(engine)
            .prop_map(|w| format!("select 1;\n{w}"))
            .boxed(),
        destructive(engine)
            .prop_map(|w| format!("{w};\nselect 1"))
            .boxed(),
        destructive(engine)
            .prop_map(|w| format!("select 'a;b'; {w}"))
            .boxed(),
    ];
    if engine == "sqlserver" {
        // T-SQL needs no `;` between statements: one batch the parser cannot read (SDB-117).
        let first = prop::sample::select(vec![
            "select * from orders",
            "SET NOCOUNT ON",
            "use shop",
            "declare @x int",
            "print 'a; drop'",
        ]);
        forms.push(
            (first, destructive(engine))
                .prop_map(|(f, w)| format!("{f}\n{w}"))
                .boxed(),
        );
    }
    Union::new(forms).prop_map(move |s| (engine, s)).boxed()
}

proptest! {
    #![proptest_config(config(512))]

    /// A write or a refused function, wherever it hides, is never a read: not for agents
    /// (`classify_read_only`), not on a read-only connection.
    #[test]
    fn a_hidden_write_or_refused_function_is_never_a_read((engine, sql) in labelled_hidden()) {
        let _turn = crate::serial::file_turn(module_path!());
        // As every caller does: by the dialect's name (MariaDB's is `mysql`).
        let dialect = dialect(engine);
        let name = dialect.name();
        prop_assert!(classify_read_only(name, &sql).is_err(), "{}: {:?} classified as a read", engine, sql);
        prop_assert!(
            classify_read_only_connection(name, &sql).is_err(),
            "{}: {:?} allowed on a read-only connection", engine, sql
        );
        prop_assert!(
            check_read_only(dialect.as_ref(), &sql).is_err(),
            "{}: {:?} passed the read-only check", engine, sql
        );
    }

    /// A `DELETE` or `UPDATE` without `WHERE`, a `DROP` or a `TRUNCATE`, behind comments or
    /// next to other statements, never passes the editor's guard unasked.
    #[test]
    fn a_destructive_statement_never_passes_the_guard_unasked(
        (engine, script) in Union::new(BARRIER_ENGINES.iter().map(|&e| hidden_destructive(e)))
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let dialect = dialect(engine);
        prop_assert!(
            check_script(dialect.as_ref(), &script, &Safeguards::default(), false).is_err(),
            "{}: {:?} passed the guard unasked", engine, script
        );
    }

    /// Any text: the classifiers and the guard never panic.
    #[test]
    fn the_barriers_never_panic(
        engine in prop::sample::select(BARRIER_ENGINES),
        sql in prop_oneof![
            any::<String>(),
            text_of("selct dropup;'\"`[]-/*!M#$\\ \n()=,1é", 48),
        ],
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let dialect = dialect(engine);
        let _ = classify_read_only(dialect.name(), &sql);
        let _ = classify_read_only_connection(dialect.name(), &sql);
        let _ = check_read_only(dialect.as_ref(), &sql);
        let _ = check_script(dialect.as_ref(), &sql, &Safeguards::default(), false);
    }
}

// ------------------------------------------------------------------------- untrusted files

/// A value of an imported column: shaped like the numbers, dates and timestamps detection
/// looks for, with any text after them (SDB-125 was a date's prefix followed by multibyte
/// characters).
fn import_value() -> S {
    prop_oneof![
        any::<String>(),
        ("[0-9A-Z]{4}-", any::<String>()).prop_map(|(a, b)| a + &b),
        (
            "20[0-9]{2}-[01][0-9]-[0-3][0-9]",
            text_of("T :0123456789.+-Zzé日🦀Молоко", 14)
        )
            .prop_map(|(a, b)| a + &b),
        "-?[0-9]{1,20}(\\.[0-9]{0,8})?([eE][+-]?[0-9]{1,3})?",
        prop::sample::select(vec!["true", "FALSE", "null", "", " ", "\u{feff}"])
            .prop_map(str::to_string),
    ]
    .boxed()
}

/// CSV text: rows of values, quoted or not, with the separators and quotes inside.
fn csv_text() -> S {
    let cell = prop_oneof![
        import_value().prop_map(|v| v.replace([',', '\n', '"'], " ")),
        import_value().prop_map(|v| format!("\"{}\"", v.replace('"', "\"\""))),
    ];
    prop::collection::vec(prop::collection::vec(cell, 1..5), 1..6)
        .prop_map(|rows| {
            rows.iter()
                .map(|r| r.join(","))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .boxed()
}

const FORMATS: &[ImportFormat] = &[
    ImportFormat::Csv,
    ImportFormat::Tsv,
    ImportFormat::Json,
    ImportFormat::Markdown,
];

proptest! {
    #![proptest_config(config(128))]

    /// Any pasted text: guessing its options and previewing it never panic, in every format.
    #[test]
    fn any_pasted_text_previews_without_panicking(
        text in prop_oneof![any::<String>(), csv_text(), text_of("[]{}\":,|-\n\t 1aé", 60)],
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let source = ImportSource::Text(text);
        let _ = preview(&source, None, 50, "postgres");
        if let Ok(guessed) = guess_options(&source) {
            for format in FORMATS {
                let options = ParseOptions { format: *format, ..guessed.clone() };
                for engine in ["postgres", "mysql", "sqlserver", "sqlite"] {
                    let _ = preview(&source, Some(&options), 50, engine);
                }
            }
        }
    }

    /// Any bytes in a file: every format, every encoding, previewed without a panic. An
    /// `.xlsx` holding anything is read as a workbook.
    #[test]
    fn any_file_previews_without_panicking(
        bytes in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..512),
            csv_text().prop_map(String::into_bytes),
        ],
        name in prop::sample::select(vec!["data.csv", "data.tsv", "data.json", "data.md", "data.xlsx"]),
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let path = temp_file(name, &bytes);
        let source = ImportSource::File(path.clone());
        let _ = preview(&source, None, 50, "postgres");
        if let Ok(guessed) = guess_options(&source) {
            for encoding in [
                SourceEncoding::Auto,
                SourceEncoding::Utf8,
                SourceEncoding::Utf16Le,
                SourceEncoding::Utf16Be,
                SourceEncoding::Windows1252,
            ] {
                let options = ParseOptions { encoding, ..guessed.clone() };
                let _ = preview(&source, Some(&options), 50, "postgres");
            }
        }
        remove_temp(&path);
    }

    /// Any bytes in a `.sql` file: read without a panic, as UTF-8, UTF-16 or Windows-1252.
    #[test]
    fn any_sql_file_reads_without_panicking(
        bytes in prop_oneof![
            prop::collection::vec(any::<u8>(), 0..512),
            any::<String>().prop_map(|s| [b"\xef\xbb\xbf".as_slice(), s.as_bytes()].concat()),
            prop::collection::vec(any::<u8>(), 0..256).prop_map(|b| [b"\xff\xfe".as_slice(), &b].concat()),
            prop::collection::vec(any::<u8>(), 0..256).prop_map(|b| [b"\xfe\xff".as_slice(), &b].concat()),
        ],
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let path = temp_file("script.sql", &bytes);
        let _ = read_sql_file(&path);
        remove_temp(&path);
    }
}

// ----------------------------------------------------------------------- SQLite, the engine

/// A SQLite statement that runs on a database with tables `t (a)` and `u (a)`: strings and
/// names that hold `;`, comments, and triggers whose bodies hold `;` and `CASE … END`.
fn sqlite_statement() -> S {
    let body =
        || text_of("ab ;-/*é日", 8).prop_map(|b| b.replace("*/", "* /").replace("--", "- -"));
    let value = prop_oneof![
        body().prop_map(|b| format!("'{}'", b.replace('\'', "''"))),
        body().prop_map(|b| format!("'it''s {b}'")),
        Just("1".to_string()),
        body().prop_map(|b| format!("'{b}' || 'x'")),
    ];
    let alias = prop_oneof![
        Just(String::new()),
        body().prop_map(|b| format!(" as \"{}\"", b.replace('"', ""))),
        body().prop_map(|b| format!(" as [{}]", b.replace(']', ""))),
        body().prop_map(|b| format!(" as `{}`", b.replace('`', ""))),
    ];
    let comment = prop_oneof![
        Just(String::new()),
        body().prop_map(|b| format!(" /* {b} */")),
        body().prop_map(|b| format!(" -- {}\n", b.replace('\n', " "))),
    ];
    let column = (value.clone(), alias, comment).prop_map(|(v, a, c)| format!("{v}{a}{c}"));
    let select =
        prop::collection::vec(column, 1..4).prop_map(|c| format!("select {}", c.join(", ")));
    let insert = value
        .clone()
        .prop_map(|v| format!("insert into t values ({v})"));
    let trigger = (
        0..1000u32,
        prop::collection::vec(value, 1..3),
    )
        .prop_map(|(n, values)| {
            let inserts: Vec<String> =
                values.iter().map(|v| format!("insert into u values ({v})")).collect();
            format!(
                "create trigger tr{n} after insert on t begin {}; update u set a = case when a = 1 then 'one;' else a end where 0; end",
                inserts.join("; ")
            )
        });
    prop_oneof![4 => select, 2 => insert, 1 => trigger].boxed()
}

fn database() -> rusqlite::Connection {
    let db = rusqlite::Connection::open_in_memory().unwrap();
    db.execute_batch("create table t (a); create table u (a);")
        .unwrap();
    db
}

/// What a database holds after a script: its schema and its rows.
fn contents(db: &rusqlite::Connection) -> Vec<String> {
    let mut out = Vec::new();
    for query in [
        "select type || ' ' || name || ' ' || coalesce(sql, '') from sqlite_master order by name",
        "select quote(a) from t order by rowid",
        "select quote(a) from u order by rowid",
    ] {
        let mut stmt = db.prepare(query).unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        out.extend(rows.map(Result::unwrap));
    }
    out
}

proptest! {
    #![proptest_config(config(128))]

    /// SQLite says where a statement ends: each span the splitter gives is exactly one complete
    /// statement to SQLite, the spans run one by one leave the database as the whole script
    /// does, and SQLite counts as many statements.
    #[test]
    fn sqlite_agrees_on_where_statements_end(
        statements in prop::collection::vec(sqlite_statement(), 1..6),
        sep in prop::sample::select(vec![";", ";\n", " ; ", ";\r\n-- next\n"]),
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        // Trigger names must differ.
        let mut seen = std::collections::HashSet::new();
        prop_assume!(statements.iter().filter(|s| s.starts_with("create trigger")).all(|s| seen.insert(s[..17].to_string())));
        let script = statements.iter().map(|s| format!("{s}{sep}")).collect::<String>();

        let whole = database();
        let mut counted = 0;
        let mut batch = rusqlite::Batch::new(&whole, &script);
        use rusqlite::fallible_iterator::FallibleIterator;
        while let Some(mut stmt) = batch.next().unwrap() {
            counted += 1;
            let mut rows = stmt.raw_query();
            while rows.next().unwrap().is_some() {}
        }

        let one_by_one = database();
        let spans = dialect("sqlite").split_statements(&script);
        for span in &spans {
            let text = span.text(&script);
            // `prepare` refuses a text that holds more than one statement.
            let mut stmt = one_by_one.prepare(text).map_err(|e| {
                TestCaseError::fail(format!("{text:?} is not one statement to SQLite: {e}"))
            })?;
            let mut rows = stmt.raw_query();
            while rows.next().unwrap().is_some() {}
        }
        prop_assert_eq!(spans.len(), counted, "{:?}", script);
        prop_assert_eq!(contents(&one_by_one), contents(&whole), "{:?}", script);
    }
}

// ------------------------------------------------- the guard's first word against the splitter

/// Safeguards that ask for nothing but what is destructive.
const ASKS_FOR_NOTHING: Safeguards = Safeguards {
    confirm_execution: false,
    confirm_data_changes: false,
    close_idle_after: None,
    manual_commit: false,
    smart_commit: false,
};

/// Whether each byte of `script` is code for the dialect's `code_ranges`.
fn code_bytes(dialect: &dyn Dialect, script: &str) -> Vec<bool> {
    let mut code = vec![false; script.len()];
    for range in dialect.code_ranges(script) {
        code[range].fill(true);
    }
    code
}

/// Whether the dialect's block comments nest, as its own `code_ranges` reads them.
fn nests_comments(dialect: &dyn Dialect) -> bool {
    let probe = "/* /* */ x */";
    let x = probe.find('x').unwrap();
    !dialect.code_ranges(probe).iter().any(|r| r.contains(&x))
}

/// The length of the block comment `text` starts with, nested or not.
fn block_comment_len(text: &str, nested: bool) -> usize {
    let b = text.as_bytes();
    let mut depth = 0usize;
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"/*") && (nested || depth == 0) {
            depth += 1;
            i += 2;
        } else if b[i..].starts_with(b"*/") {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i;
            }
        } else {
            i += 1;
        }
    }
    b.len()
}

/// The first word of the statement `span` of `script`, read the splitter's way, as
/// `spelaion-dialect`'s split properties read it: after white space and comments, the first run
/// of letters, digits and `_` at a byte the dialect's code ranges call code, lowercased; a
/// string, a quoted name or any other character first gives an empty word.
fn splitter_first_word(dialect: &dyn Dialect, script: &str, span: &StatementSpan) -> String {
    let code = code_bytes(dialect, script);
    let nested = nests_comments(dialect);
    let mut i = span.start;
    while i < span.end {
        let rest = &script[i..span.end];
        let c = rest.chars().next().expect("inside the span");
        if code[i] {
            if c.is_whitespace() {
                i += c.len_utf8();
                continue;
            }
            return rest
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect::<String>()
                .to_lowercase();
        }
        if ["--", "#", "//"].iter().any(|o| rest.starts_with(o)) {
            i += rest.find('\n').map_or(rest.len(), |n| n + 1);
        } else if rest.starts_with("/*") {
            i += block_comment_len(rest, nested);
        } else {
            return String::new();
        }
    }
    String::new()
}

/// What may come before a statement's first word: white space of every kind, each dialect's
/// comments, strings and names, separators.
fn before_a_statement() -> S {
    const PIECES: &[&str] = &[
        " ",
        "\n",
        "\t",
        "\r\n",
        "\u{a0}",
        "\u{b}",
        "\u{85}",
        "\u{3000}",
        "-- c\n",
        "--c\n",
        "--\n",
        "# c\n",
        "#c\n",
        "#!c\n",
        "// c\n",
        "/* c */",
        "/* a /* b */ c */",
        "/*! c */",
        "/*M! c */",
        "'s' ",
        "\"n\" ",
        "`n` ",
        "[n] ",
        "$$b$$ ",
        ";",
        ";\n",
        "(",
        "\nGO\n",
        "\n/\n",
        "select 1;\n",
        "x ",
    ];
    prop::collection::vec(prop::sample::select(PIECES), 0..6)
        .prop_map(|p| p.concat())
        .boxed()
}

/// Statements the guard asks about from their first word alone: a `DROP` always, and a
/// `TRUNCATE`, `DELETE` or `UPDATE` the SQL parser cannot read (the `((` keeps it from it).
const RISKY: &[(&str, &str)] = &[
    ("drop", "drop table t"),
    ("truncate", "truncate table t (("),
    ("delete", "delete from t (("),
    ("update", "update t set (("),
];

/// Disagreements this property has found, each with the bug card that fixes it (ADR-065,
/// step 1): `(labels, card, shape)`. Only a statement of the known shape is let through.
type GuardKnown = (&'static [&'static str], &'static str, fn(&str) -> bool);

const GUARD_KNOWN: &[GuardKnown] = &[
    // `# c\ndrop table t`: the guard's first word skips comments the engine does not have:
    // `#` on every engine (PostgreSQL's `#` is an operator), and MySQL's `--` with no white
    // space after it. It asks about a DROP the splitter does not read first: more, never less.
    (
        &[
            "postgres",
            "sqlite",
            "duckdb",
            "trino",
            "databend",
            "flightsql",
            "mysql",
            "mysql[NO_BACKSLASH_ESCAPES]",
            "mysql[ANSI_QUOTES]",
        ],
        "SDB-517",
        |statement| {
            let before = before_risky(statement);
            before.contains('#')
                || before
                    .match_indices("--")
                    .any(|(at, _)| !before[at + 2..].starts_with(char::is_whitespace))
        },
    ),
    // `/* a /* b */ c */truncate table t ((`: the guard's first word ends a block comment at
    // its first `*/`, and the SQL parser reads block comments nested on every engine. Where the
    // dialect nests them and the parser cannot read the statement (PostgreSQL, DuckDB,
    // ClickHouse; DuckDB even for a DROP), the destructive statement the splitter reads first
    // is not asked about; where the dialect does not nest them (Trino, Databend, Flight SQL),
    // the guard asks about a DROP that is not the first word.
    (
        &[
            "postgres",
            "duckdb",
            "clickhouse",
            "trino",
            "databend",
            "flightsql",
        ],
        "SDB-518",
        |statement| before_risky(statement).matches("/*").count() >= 2,
    ),
];

/// The text of a generated statement before its risky word.
fn before_risky(statement: &str) -> &str {
    let lower = statement.to_ascii_lowercase();
    let at = RISKY
        .iter()
        .filter_map(|(word, _)| lower.rfind(word))
        .max()
        .unwrap_or(statement.len());
    &statement[..at]
}

fn guard_known(label: &str, statement: &str) -> bool {
    GUARD_KNOWN.iter().any(|(labels, card, shape)| {
        labels.contains(&label) && !card.is_empty() && shape(statement)
    })
}

proptest! {
    #![proptest_config(config(512))]

    /// The guard reads a statement's first word as the splitter does (ADR-065, step 1): a
    /// statement whose first word, read the splitter's way, is `DROP` (or an unreadable
    /// `TRUNCATE`, `DELETE`, `UPDATE`) is one the guard asks about; and where the guard asks about
    /// a `DROP` from the statement alone, its first word is `DROP`. SQL Server, Oracle,
    /// ClickHouse and HiveQL's engines read more of an unreadable statement than its first word,
    /// so only the first half holds for them; CQL and TDengine have lists of their own.
    #[test]
    fn the_guard_reads_the_splitters_first_word(
        before in before_a_statement(),
        risky in prop::sample::select(RISKY),
    ) {
        let _turn = crate::serial::file_turn(module_path!());
        let (word, statement) = risky;
        let script = format!("{before}{statement}");
        for (label, dialect) in sql_dialects() {
            if dialect.statement_class("select 1").is_some() {
                continue;
            }
            let reads_more = matches!(label.as_str(), "sqlserver" | "oracle" | "clickhouse" | "hive");
            let risks = script_risks(dialect.as_ref(), &script, &ASKS_FOR_NOTHING);
            for (index, span) in dialect.split_statements(&script).iter().enumerate() {
                let text = span.text(&script);
                let first = splitter_first_word(dialect.as_ref(), &script, span);
                let risk = risks.iter().find(|r| r.index == index).map(|r| r.risk);
                if guard_known(&label, text) {
                    continue;
                }
                if first == word && text.ends_with(statement) {
                    prop_assert!(
                        risk.is_some(),
                        "{}: the guard does not ask about {:?}, whose first word is {:?}",
                        label,
                        text,
                        word
                    );
                }
                if !reads_more && risk == Some(Risk::Drop) {
                    prop_assert_eq!(
                        first.as_str(),
                        "drop",
                        "{}: the guard asks about a DROP in {:?}",
                        label,
                        text
                    );
                }
            }
        }
    }
}
