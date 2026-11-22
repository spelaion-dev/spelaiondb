//! Property tests of every dialect's splitter (SDB-156).
//!
//! The example tests next to each dialect cover what their author thought of. These generate
//! the inputs: scripts built from the tricky pieces of each dialect (quotes and their escapes,
//! nested comments, `$tag$` bodies, bracket names, `BEGIN … END`, `DELIMITER`, `GO`, executable
//! comments, PL/SQL units, `COPY` data) must split back into the statements they were built
//! from, and any text at all must split without a panic into spans that never overlap and that
//! leave out nothing but comments, white space and separators.
//!
//! In CI the cases are bounded and the seed is fixed, so a run is the same every time and a
//! failure prints its smallest input. `SPELAION_FUZZ_CASES=<n>` runs `n` cases per property
//! with a random seed (`PROPTEST_RNG_SEED` fixes it), for long runs on a machine or on a
//! schedule, never in a pull request's job.

use std::ops::Range;
use std::sync::Arc;

use proptest::prelude::*;
use proptest::strategy::Union;
use proptest::test_runner::{Config, RngSeed};
use spelaion_dialect::{MysqlDialect, for_engine};
use spelaion_driver_api::{Dialect, StatementSpan};

// ------------------------------------------------------------------------------------ setup

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

/// Every engine name the configuration may give (`for_engine`).
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

/// The `sql_mode`s that read MySQL text otherwise than the default.
const MYSQL_MODES: &[&str] = &[
    "NO_BACKSLASH_ESCAPES",
    "ANSI_QUOTES",
    "ANSI,NO_BACKSLASH_ESCAPES",
];

fn dialect(engine: &str) -> Arc<dyn Dialect> {
    for_engine(engine).unwrap_or_else(|| panic!("{engine} has no dialect"))
}

fn mysql_mode(mode: &str) -> Arc<dyn Dialect> {
    MysqlDialect
        .with_sql_mode(mode)
        .expect("MySQL reads every mode")
}

/// Every dialect, with a label: one per engine, and MySQL under each `sql_mode` that changes how
/// its text is read.
fn every_dialect() -> Vec<(String, Arc<dyn Dialect>)> {
    let mut all: Vec<(String, Arc<dyn Dialect>)> = ENGINES
        .iter()
        .map(|engine| (engine.to_string(), dialect(engine)))
        .collect();
    for mode in MYSQL_MODES {
        all.push((format!("mysql[{mode}]"), mysql_mode(mode)));
    }
    all
}

// ------------------------------------------------------------------------------- invariants

/// What holds for any text: spans and code ranges inside the text, on character boundaries,
/// in order and never overlapping; spans not empty and without white space around them; a
/// `COPY`'s data after its statement and before the next one.
fn check_shape(label: &str, dialect: &dyn Dialect, script: &str) -> Result<(), TestCaseError> {
    let spans = dialect.split_statements(script);
    let mut last_end = 0;
    for span in &spans {
        prop_assert!(
            span.start < span.end && span.end <= script.len(),
            "{label}: span {span:?} out of the text"
        );
        prop_assert!(
            script.is_char_boundary(span.start) && script.is_char_boundary(span.end),
            "{label}: span {span:?} cuts a character"
        );
        prop_assert!(
            span.start >= last_end,
            "{label}: span {span:?} overlaps the one before"
        );
        let text = span.text(script);
        prop_assert_eq!(
            text,
            text.trim(),
            "{}: span with white space around it",
            label
        );
        last_end = span.end;
        // A psql meta-command is given data it does not have, for now: see
        // `a_psql_meta_command_has_no_copy_data`.
        if text.starts_with('\\') {
            continue;
        }
        if let Some(data) = dialect.copy_data(script, span) {
            prop_assert!(
                span.end <= data.start && data.start <= data.end && data.end <= script.len(),
                "{label}: COPY data {data:?} out of place after {span:?}"
            );
            prop_assert!(
                script.is_char_boundary(data.start) && script.is_char_boundary(data.end),
                "{label}: COPY data {data:?} cuts a character"
            );
            last_end = last_end.max(data.end);
        }
    }
    let mut last_end = 0;
    for (n, range) in dialect.code_ranges(script).into_iter().enumerate() {
        prop_assert!(
            range.start < range.end && range.end <= script.len(),
            "{label}: code range {range:?} out of the text or empty"
        );
        prop_assert!(
            script.is_char_boundary(range.start) && script.is_char_boundary(range.end),
            "{label}: code range {range:?} cuts a character"
        );
        prop_assert!(
            n == 0 || range.start > last_end,
            "{label}: code range {range:?} overlaps or touches the one before"
        );
        last_end = range.end;
    }
    Ok(())
}

/// Whether a line left between statements is one of the dialect's separators, which are not
/// statements: SQL Server's `GO [count]`, Oracle's `/`, a psql meta-command PostgreSQL skips
/// (`\restrict`) or a `COPY` data terminator (`\.`), MySQL's `DELIMITER x`.
fn separator_line(label: &str, line: &str) -> bool {
    let line = line.trim();
    let mut words = line.split_whitespace();
    match label {
        "sqlserver" => {
            words.next().is_some_and(|w| w.eq_ignore_ascii_case("go"))
                && words
                    .next()
                    .is_none_or(|n| n.bytes().all(|c| c.is_ascii_digit()))
                && words.next().is_none()
        }
        "oracle" => line == "/",
        "postgres" => line.starts_with('\\'),
        _ if label.starts_with("mysql") || label == "mariadb" => words
            .next()
            .is_some_and(|w| w.eq_ignore_ascii_case("delimiter")),
        _ => false,
    }
}

/// The text the splitter left out of every statement that is neither white space, a comment,
/// a `;` nor a separator line: the first such piece, or `None`. Comments are told by the
/// dialect's own code ranges, so this checks the splitter against the reading the placeholder
/// scanner uses. For MySQL a standalone executable comment (`/*! … */`, which the server runs)
/// counts as text left out (SDB-113).
fn left_out(label: &str, dialect: &dyn Dialect, script: &str) -> Option<String> {
    let mut covered: Vec<Range<usize>> = Vec::new();
    for span in dialect.split_statements(script) {
        covered.push(span.start..span.end);
        if let Some(data) = dialect.copy_data(script, &span) {
            // With its `\.` line, which is neither SQL nor data.
            let rest = &script[data.end..];
            let end = if rest.starts_with("\\.") {
                data.end + rest.find('\n').map_or(rest.len(), |n| n + 1)
            } else {
                data.end
            };
            covered.push(data.start..end);
        }
    }
    covered.sort_by_key(|r| r.start);
    let mut gaps = Vec::new();
    let mut at = 0;
    for range in &covered {
        if range.start > at {
            gaps.push(at..range.start);
        }
        at = at.max(range.end);
    }
    if at < script.len() {
        gaps.push(at..script.len());
    }
    let code = dialect.code_ranges(script);
    let in_code = |i: usize| code.iter().any(|r| r.contains(&i));
    let mysql = label.starts_with("mysql") || label == "mariadb";
    // CQL and TDengine send no statement without a word (a lone string or `$$` body, a lone
    // sign), which their servers would refuse anyway.
    let wordless_allowed = matches!(label, "cassandra" | "tdengine");
    // A custom delimiter can be any text: what is left between statements is not checked then.
    let custom_delimiter = mysql && script.to_ascii_lowercase().contains("delimiter");
    for gap in &gaps {
        if !custom_delimiter {
            let masked: String = script[gap.clone()]
                .char_indices()
                .map(|(i, c)| match c {
                    '\n' => '\n',
                    _ if in_code(gap.start + i) => c,
                    _ => ' ',
                })
                .collect();
            let stray = masked
                .split('\n')
                .filter(|line| !separator_line(label, line))
                .flat_map(str::chars)
                .any(|c| {
                    if wordless_allowed {
                        // Their words are ASCII, as their servers' names are.
                        c.is_ascii_alphanumeric() || c == '_'
                    } else {
                        !c.is_whitespace() && c != ';'
                    }
                });
            if stray {
                return Some(script[gap.clone()].to_string());
            }
        }
        // What is not code and starts in the gap must be a comment, not a string or a name.
        let mut i = gap.start;
        while i < gap.end {
            let rest = &script[i..];
            let width = rest.chars().next().map_or(1, char::len_utf8);
            if in_code(i) || (i > 0 && !in_code(i - 1)) {
                i += width;
                continue;
            }
            let comment = ["--", "/*", "#", "//"].iter().any(|o| rest.starts_with(o));
            let executable = mysql && {
                let head = rest.as_bytes();
                head.starts_with(b"/*!")
                    || (head.len() >= 4
                        && head.starts_with(b"/*")
                        && head[2].eq_ignore_ascii_case(&b'm')
                        && head[3] == b'!')
            };
            if (!comment || executable) && !wordless_allowed {
                return Some(script[gap.clone()].to_string());
            }
            i += width;
        }
    }
    None
}

// ------------------------------------------------------------------------------- generators

type S = BoxedStrategy<String>;

/// Text made of `chars`, up to `max` of them.
fn text_of(chars: &str, max: usize) -> S {
    let chars: Vec<char> = chars.chars().collect();
    prop::collection::vec(prop::sample::select(chars), 0..max)
        .prop_map(|v| v.into_iter().collect())
        .boxed()
}

/// What goes in a quoted text or a comment: every dialect's separators and the openers of its
/// comments, quotes and bodies, line breaks and multibyte characters. Never a quote, a
/// backslash, `*` or `/`: each kind of text adds the ones it may hold.
const BODY: &str = "ab ;:#$@[]{}()<>!-.=go\n\té日🦀";
/// The body of a line comment: no line break.
const LINE: &str = "ab ;:#$@[]{}()<>!-./*'\"`=goé日";

fn single_quoted() -> S {
    text_of(&format!("{BODY}\"`*/"), 12)
        .prop_map(|b| format!("'{b}'"))
        .boxed()
}

fn double_quoted() -> S {
    text_of(&format!("{BODY}'`*/"), 12)
        .prop_map(|b| format!("\"{b}\""))
        .boxed()
}

fn line_comment() -> S {
    text_of(LINE, 12).prop_map(|b| format!("-- {b}\n")).boxed()
}

fn block_comment() -> S {
    text_of(&format!("{BODY}'\"`"), 12)
        .prop_map(|b| format!("/* {b} */"))
        .boxed()
}

/// A block comment with one nested inside it, for the dialects where they nest.
fn nested_comment() -> S {
    let body = || text_of(&format!("{BODY}'\"`"), 8);
    (body(), body(), body())
        .prop_map(|(a, b, c)| format!("/* {a} /* {b} */ {c} */"))
        .boxed()
}

/// Words that are nothing special to any splitter (no `BEGIN`, `END`, `CASE`, `GO`, `DECLARE`,
/// `DELIMITER`, `CREATE`, `MERGE`, `COPY`, `BATCH`; no `e`, `n` or `q` that a quote could join).
const WORDS: &[&str] = &[
    "from", "where", "t", "x", "y1", "and", "or", "not", "null", "1", "42", "0.5", "as", "on",
    "is", "in",
];
const OPS: &[&str] = &["=", "<>", "+", "*", ",", "(", ")", "."];

fn plain_atom() -> S {
    prop_oneof![
        prop::sample::select(WORDS).prop_map(str::to_string),
        prop::sample::select(OPS).prop_map(str::to_string),
    ]
    .boxed()
}

/// What every SQL dialect reads alike: words, `'…'`, `"…"` (a string or a name), `--` and
/// `/* */` comments.
fn common_atoms() -> Vec<(u32, S)> {
    vec![
        (6, plain_atom()),
        (2, single_quoted()),
        (1, double_quoted()),
        (1, line_comment()),
        (1, block_comment()),
    ]
}

/// A string whose body holds `\'` and `\\`, for the dialects where a backslash escapes.
fn backslash_string(quote: char) -> S {
    let plain: Vec<char> = format!("{BODY}*/").chars().collect();
    let piece = prop_oneof![
        3 => prop::sample::select(plain).prop_map(String::from),
        1 => Just(format!("\\{quote}")),
        1 => Just("\\\\".to_string()),
    ];
    prop::collection::vec(piece, 0..10)
        .prop_map(move |p| format!("{quote}{}{quote}", p.concat()))
        .boxed()
}

/// A quoted text whose closing character, doubled, stands for itself.
fn doubled(open: char, close: char) -> S {
    let plain: Vec<char> = format!("{BODY}*/").replace(close, "").chars().collect();
    let piece = prop_oneof![
        3 => prop::sample::select(plain).prop_map(String::from),
        1 => Just(format!("{close}{close}")),
    ];
    prop::collection::vec(piece, 0..10)
        .prop_map(move |p| format!("{open}{}{close}", p.concat()))
        .boxed()
}

/// A `$$ … $$` body, holding no `$`.
fn dollar_body() -> S {
    text_of(&format!("{BODY}'\"`*/").replace('$', ""), 12)
        .prop_map(|b| format!("$${b}$$"))
        .boxed()
}

/// A `$tag$ … $tag$` body, which may hold `$$` and other tags.
fn tagged_body() -> S {
    let plain: Vec<char> = format!("{BODY}'\"`*/").replace('$', "").chars().collect();
    let piece = prop_oneof![
        4 => prop::sample::select(plain).prop_map(String::from),
        1 => Just("$$".to_string()),
        1 => Just("$b$".to_string()),
    ];
    (
        prop::sample::select(vec!["a", "body", "x_1"]),
        prop::collection::vec(piece, 0..10),
    )
        .prop_map(|(tag, p)| format!("${tag}${}${tag}$", p.concat()))
        // The body must not close it early (`$a$` + `$$a` + `$a$` holds `$a$` inside).
        .prop_filter("a body that holds its own tag", |s| {
            let tag = &s[..=s[1..].find('$').map_or(0, |p| p + 1)];
            s[tag.len()..].find(tag) == Some(s.len() - 2 * tag.len())
        })
        .boxed()
}

/// What each dialect adds to [`common_atoms`]: its own quotes, escapes, comments and bodies.
fn extra_atoms(label: &str) -> Vec<(u32, S)> {
    match label {
        "postgres" | "duckdb" => {
            let mut extra = vec![
                (
                    1,
                    backslash_string('\'').prop_map(|s| format!("E{s}")).boxed(),
                ),
                (1, dollar_body()),
                (1, tagged_body()),
                (1, nested_comment()),
                (1, Just("$1".to_string()).boxed()),
                (1, Just("x::int".to_string()).boxed()),
            ];
            if label == "duckdb" {
                extra.push((
                    1,
                    single_quoted().prop_map(|k| format!("{{{k}: 1}}")).boxed(),
                ));
            }
            extra
        }
        "mysql" | "mariadb" => vec![
            (1, backslash_string('\'')),
            (1, backslash_string('"')),
            (1, doubled('`', '`')),
            (
                1,
                text_of(LINE, 10).prop_map(|b| format!("# {b}\n")).boxed(),
            ),
        ],
        "mysql[NO_BACKSLASH_ESCAPES]" => vec![
            // A backslash is an ordinary character: `'C:\'` is a whole string.
            (
                1,
                single_quoted()
                    .prop_map(|s| s.replace('\'', "") + "\\")
                    .prop_map(|b| format!("'{b}'"))
                    .boxed(),
            ),
            (1, doubled('\'', '\'')),
            (1, doubled('`', '`')),
        ],
        "mysql[ANSI_QUOTES]" => vec![
            (1, backslash_string('\'')),
            // `"…"` is a name: a backslash does not escape in it.
            (
                1,
                doubled('"', '"')
                    .prop_map(|s| format!("{}\\\"", &s[..s.len() - 1]))
                    .boxed(),
            ),
            (1, doubled('`', '`')),
        ],
        "mysql[ANSI,NO_BACKSLASH_ESCAPES]" => {
            vec![(1, doubled('\'', '\'')), (1, doubled('"', '"'))]
        }
        "sqlserver" => vec![
            (1, doubled('[', ']')),
            (1, doubled('\'', '\'').prop_map(|s| format!("N{s}")).boxed()),
            (1, nested_comment()),
            (
                1,
                single_quoted()
                    .prop_map(|s| format!("case when x = 1 then {s} else 2 end"))
                    .boxed(),
            ),
        ],
        "sqlite" | "libsql" => vec![
            (
                1,
                text_of(&format!("{BODY}'\"`*/").replace(']', ""), 10)
                    .prop_map(|b| format!("[{b}]"))
                    .boxed(),
            ),
            (1, doubled('`', '`')),
            (1, doubled('\'', '\'')),
        ],
        "oracle" => {
            let q = |open: char, close: char| {
                text_of(&format!("{BODY}'\"`*/").replace(close, ""), 10)
                    .prop_map(move |b| format!("q'{open}{b}{close}'"))
                    .boxed()
            };
            vec![
                (1, q('[', ']')),
                (1, q('{', '}')),
                (1, q('<', '>')),
                (1, q('!', '!')),
                (1, q('(', ')').prop_map(|s| format!("n{s}")).boxed()),
                (1, doubled('\'', '\'').prop_map(|s| format!("n{s}")).boxed()),
                (1, doubled('"', '"')),
            ]
        }
        "clickhouse" => vec![
            (1, backslash_string('\'')),
            (1, backslash_string('`')),
            (
                1,
                text_of(LINE, 10).prop_map(|b| format!("# {b}\n")).boxed(),
            ),
            (1, nested_comment()),
            (1, tagged_body()),
        ],
        "databend" => vec![
            (1, backslash_string('\'')),
            (
                1,
                text_of(&format!("{BODY}'\"*/"), 10)
                    .prop_map(|b| format!("`{b}`"))
                    .boxed(),
            ),
            (1, dollar_body()),
        ],
        "spanner" => vec![
            (1, backslash_string('\'')),
            (1, backslash_string('`')),
            (
                1,
                text_of(LINE, 10).prop_map(|b| format!("# {b}\n")).boxed(),
            ),
            (
                1,
                text_of(&format!("{BODY}\"`*/"), 10)
                    .prop_map(|b| format!("'''{b}'''"))
                    .boxed(),
            ),
            (
                1,
                text_of(&format!("{BODY}'`*/"), 10)
                    .prop_map(|b| format!("\"\"\"{b}\"\"\""))
                    .boxed(),
            ),
        ],
        "trino" | "presto" | "flightsql" | "dremio" | "gizmosql" => {
            vec![(1, doubled('\'', '\'')), (1, doubled('"', '"'))]
        }
        "hive" | "spark" | "kyuubi" | "impala" => vec![
            (1, backslash_string('\'')),
            (1, backslash_string('"')),
            (1, doubled('`', '`')),
        ],
        "cassandra" => vec![
            (
                1,
                text_of(LINE, 10).prop_map(|b| format!("// {b}\n")).boxed(),
            ),
            (1, dollar_body()),
            (1, doubled('\'', '\'')),
            (1, doubled('"', '"')),
        ],
        "tdengine" => vec![
            (1, backslash_string('\'')),
            (1, backslash_string('"')),
            (1, doubled('`', '`')),
        ],
        _ => Vec::new(),
    }
}

fn atom(label: &str) -> S {
    let atoms: Vec<(u32, S)> = common_atoms()
        .into_iter()
        .chain(extra_atoms(label))
        .collect();
    Union::new_weighted(atoms).boxed()
}

/// A statement of `first` followed by fewer than `max` of `atom`. It keeps the line break that
/// ends a `--` comment at its end, so a `;` written after it is not in the comment; the span
/// is the statement without it ([`expected`]).
fn statement_with(first: &'static str, atom: S, max: usize) -> S {
    prop::collection::vec(atom, 0..max)
        .prop_map(move |atoms| {
            let mut text = first.to_string();
            for a in atoms {
                text.push(' ');
                text.push_str(&a);
            }
            text
        })
        .boxed()
}

/// The span a generated statement is expected to have: without the white space at its end.
fn expected(text: &str) -> String {
    text.trim().to_string()
}

fn statement(label: &str) -> S {
    statement_with("select", atom(label), 9)
}

/// What ends a statement: a `;` with or without white space around it.
fn separator() -> BoxedStrategy<&'static str> {
    prop::sample::select(vec![";", ";\n", " ;\n\n", ";\r\n", "\n;\n", ";  "]).boxed()
}

/// A script of statements from `statement`, each ended by a separator, and the statements.
fn script_of(statement: S) -> BoxedStrategy<(Vec<String>, String)> {
    prop::collection::vec((statement, separator()), 1..6)
        .prop_map(|items| {
            let mut script = String::new();
            let mut spans = Vec::new();
            for (text, sep) in items {
                script.push_str(&text);
                script.push_str(sep);
                spans.push(expected(&text));
            }
            (spans, script)
        })
        .boxed()
}

fn texts<'a>(spans: &[StatementSpan], script: &'a str) -> Vec<&'a str> {
    spans.iter().map(|s| s.text(script)).collect()
}

/// The labels whose language is SQL or shaped like it: the round trip applies to them.
fn sql_labels() -> Vec<String> {
    every_dialect()
        .into_iter()
        .filter(|(_, d)| !d.reads_requests())
        .map(|(label, _)| label)
        .collect()
}

fn dialect_of(label: &str) -> Arc<dyn Dialect> {
    match label.strip_prefix("mysql[") {
        Some(mode) => mysql_mode(mode.trim_end_matches(']')),
        None => dialect(label),
    }
}

/// Labels and scripts built from each label's own pieces.
fn labelled_script() -> BoxedStrategy<(String, Vec<String>, String)> {
    let per_label: Vec<BoxedStrategy<(String, Vec<String>, String)>> = sql_labels()
        .into_iter()
        .map(|label| {
            let l = label.clone();
            script_of(statement(&label))
                .prop_map(move |(expected, script)| (l.clone(), expected, script))
                .boxed()
        })
        .collect();
    Union::new(per_label).boxed()
}

/// Any text, weighted towards the pieces splitters care about.
fn soup() -> S {
    const PIECES: &[&str] = &[
        "'",
        "\"",
        "`",
        "[",
        "]",
        "--",
        "-- ",
        "#",
        "# ",
        "#!",
        "/*",
        "*/",
        "/*!",
        "/*!50001 ",
        "/*M!",
        "//",
        ";",
        ";;",
        "$",
        "$$",
        "$a$",
        "$1",
        "E'",
        "e'",
        "N'",
        "\\",
        "\\'",
        "\\\\",
        "\n",
        "\r\n",
        " ",
        "\t",
        "GO",
        "go 2",
        "\nGO\n",
        "DELIMITER $$\n",
        "delimiter ;;\n",
        "BEGIN",
        "begin atomic",
        "begin tran",
        "END",
        "CASE",
        "create function f() ",
        "create procedure p ",
        "create trigger tr ",
        "declare @x int",
        "merge",
        "\n/\n",
        "q'[",
        "q'{",
        "nq'<",
        "copy t from stdin;\n",
        "\\.\n",
        "\\restrict k\n",
        "\\set x\n",
        "'''",
        "\"\"\"",
        "{",
        "}",
        "(",
        ")",
        ":",
        "::",
        ":x",
        "@",
        "select",
        "begin batch",
        "apply batch",
        "é",
        "日本",
        "🦀",
        "\u{0}",
        "\u{feff}",
        "\u{a0}",
        "\u{b}",
        "\u{85}",
        "\u{2028}",
        "\u{3000}",
    ];
    let piece = prop_oneof![
        6 => prop::sample::select(PIECES).prop_map(str::to_string),
        2 => "[a-z]{1,6}",
        1 => any::<char>().prop_map(String::from),
    ];
    prop_oneof![
        8 => prop::collection::vec(piece, 0..48).prop_map(|p| p.concat()),
        1 => any::<String>(),
    ]
    .boxed()
}

// --------------------------------------------------------------------------- the properties

/// White space that `is_ascii_whitespace` does not know: a no-break space, a vertical tab, a
/// form feed's Unicode cousins, U+2028, U+3000.
const UNICODE_WHITE_SPACE: &[&str] = &["\u{a0}", "\u{b}", "\u{85}", "\u{2028}", "\u{3000}"];

/// Found by `any_text_splits_into_spans_that_lose_nothing` (SDB-434). A statement that held
/// only white space `is_ascii_whitespace` does not know (a no-break space pasted after the
/// last `;`) counted as code and was then trimmed with `str::trim`, which removed it: the span
/// ended before it started, and `StatementSpan::text` panicked on it. Every splitter now reads
/// white space by one rule, and a statement of white space alone is no statement.
#[test]
fn unicode_white_space_alone_is_no_statement() {
    let mut wrong = Vec::new();
    for (label, dialect) in every_dialect() {
        for space in UNICODE_WHITE_SPACE {
            for script in [space.to_string(), format!("{space}\n{space}")] {
                let spans = dialect.split_statements(&script);
                if !spans.is_empty() {
                    wrong.push(format!("{label}: {script:?} -> {spans:?}"));
                }
            }
            // `;` separates statements only in SQL and the languages shaped like it.
            if dialect.reads_requests() {
                continue;
            }
            for script in [format!(";{space};"), format!("{space};\n;{space}")] {
                let spans = dialect.split_statements(&script);
                if !spans.is_empty() {
                    wrong.push(format!("{label}: {script:?} -> {spans:?}"));
                }
            }
            for script in [
                format!("select 1;{space}"),
                format!("select 1;{space}\n"),
                format!("{space}select 1{space};{space}"),
            ] {
                let spans = dialect.split_statements(&script);
                if spans.iter().any(|s| s.start > s.end) || texts(&spans, &script) != ["select 1"] {
                    wrong.push(format!("{label}: {script:?} -> {spans:?}"));
                }
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// Found by `any_text_splits_into_spans_that_lose_nothing`. A psql `\copy … from stdin;` line
/// is a meta-command, a statement of its own the session refuses. When the next thing in the
/// script is a `;`, `copy_data` still gives it the text after that `;` as its data, over the
/// statements the splitter reads there. The session asks for `copy_data` before it refuses a
/// meta-command, so it sends the `\copy` line to the server as a `COPY` (which refuses it) and
/// also runs that text as SQL.
#[test]
#[ignore = "found by SDB-156's property tests: copy_data gives data to a psql meta-command; its fix gets its own Plane item"]
fn a_psql_meta_command_has_no_copy_data() {
    let dialect = dialect("postgres");
    let script = "\\copy t from stdin;\n;\nselect 1";
    let spans = dialect.split_statements(script);
    assert_eq!(texts(&spans, script), ["\\copy t from stdin;", "select 1"]);
    assert_eq!(dialect.copy_data(script, &spans[0]), None);
}

/// Found by `any_text_splits_into_spans_that_lose_nothing`. The splitter reads a psql
/// meta-command line (`\echo it's`) as one statement of its own, which the session refuses,
/// and the SQL after it as SQL. `code_ranges` reads that line as SQL: its `'` opens a string
/// that hides the next statement's `:x` from the placeholder scanner (the statement is then
/// sent with `:x` in it), and a comment's text can become code. Same family as SDB-135; fixed
/// by SDB-436.
#[test]
fn postgres_code_ranges_read_meta_command_lines_as_the_splitter_does() {
    let dialect = dialect("postgres");
    let script = "\\echo it's\nselect :x";
    let spans = dialect.split_statements(script);
    assert_eq!(texts(&spans, script), ["\\echo it's", "select :x"]);
    let placeholder = script.find(":x").unwrap();
    assert!(
        dialect
            .code_ranges(script)
            .iter()
            .any(|r| r.contains(&placeholder)),
        "`:x` is code to the splitter but not to code_ranges"
    );
}

proptest! {
    #![proptest_config(config(512))]

    /// Any text: no panic, spans and code ranges in shape, nothing but comments, white space
    /// and separators left out, on every dialect.
    #[test]
    fn any_text_splits_into_spans_that_lose_nothing(script in soup()) {
        for (label, dialect) in every_dialect() {
            check_shape(&label, dialect.as_ref(), &script)?;
            if dialect.reads_requests() {
                continue;
            }
            let lost = left_out(&label, dialect.as_ref(), &script);
            prop_assert!(lost.is_none(), "{}: {:?} is left out of every statement", label, lost);
        }
    }
}

proptest! {
    #![proptest_config(config(256))]

    /// Statements built from each dialect's own quotes, escapes, comments and bodies, joined
    /// with `;`, split back into those statements.
    #[test]
    fn statements_of_each_dialect_split_back((label, expected, script) in labelled_script()) {
        let dialect = dialect_of(&label);
        check_shape(&label, dialect.as_ref(), &script)?;
        let spans = dialect.split_statements(&script);
        prop_assert_eq!(texts(&spans, &script), expected, "{}", label);
    }
}

// ----------------------------------------------------------------------------- PostgreSQL

/// A piece of a PostgreSQL script: a statement, a `COPY … FROM STDIN` with its data, or a psql
/// meta-command on a line of its own.
#[derive(Debug, Clone)]
enum PgPiece {
    Statement(String, &'static str),
    Copy(String),
    Meta(String),
}

fn pg_inner(label: &'static str) -> S {
    statement_with("select", atom(label), 6)
}

/// A `CREATE FUNCTION | PROCEDURE … BEGIN ATOMIC … END` (SDB-114): `;` inside its body.
fn pg_routine() -> S {
    let case = single_quoted().prop_map(|s| format!("case when x then {s} else 'b' end"));
    let inner =
        prop_oneof![3 => pg_inner("postgres"), 1 => case.prop_map(|c| format!("select {c}"))];
    (
        prop::sample::select(vec![
            "create function f() returns int language sql",
            "CREATE OR REPLACE FUNCTION f(a int) RETURNS int LANGUAGE sql",
            "create procedure p() language sql",
            "create or replace procedure p()",
        ]),
        prop::collection::vec(inner, 1..4),
    )
        .prop_map(|(head, inner)| format!("{head} begin atomic {}; end", inner.join("; ")))
        .boxed()
}

fn pg_piece() -> BoxedStrategy<PgPiece> {
    let data_line = text_of(&format!("{BODY}'\"`*/\\").replace('\n', ""), 16)
        .prop_filter("a data line is not the terminator", |l| l != "\\.");
    prop_oneof![
        6 => (pg_inner("postgres"), separator()).prop_map(|(s, sep)| PgPiece::Statement(s, sep)),
        2 => (pg_routine(), separator()).prop_map(|(s, sep)| PgPiece::Statement(s, sep)),
        1 => (prop::sample::select(vec!["begin", "end", "begin work", "END WORK", "commit"]), separator())
            .prop_map(|(s, sep)| PgPiece::Statement(s.to_string(), sep)),
        1 => prop::collection::vec(data_line, 0..4).prop_map(|lines| {
            PgPiece::Copy(lines.into_iter().map(|l| l + "\n").collect())
        }),
        1 => prop::sample::select(vec!["\\set x 1", "\\connect other", "\\restrict abc", "\\unrestrict abc"])
            .prop_map(|m| PgPiece::Meta(m.to_string())),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(config(256))]

    /// PostgreSQL scripts with `BEGIN ATOMIC` routines, transaction words, `COPY` data and
    /// psql meta-commands split back into their statements, the data given apart.
    #[test]
    fn postgres_scripts_split_back(pieces in prop::collection::vec(pg_piece(), 1..7)) {
        let mut script = String::new();
        let mut expected: Vec<String> = Vec::new();
        let mut data: Vec<String> = Vec::new();
        for piece in pieces {
            match piece {
                PgPiece::Statement(text, sep) => {
                    script.push_str(&text);
                    script.push_str(sep);
                    expected.push(self::expected(&text));
                }
                PgPiece::Copy(rows) => {
                    expected.push("copy t from stdin".into());
                    script.push_str("copy t from stdin;\n");
                    script.push_str(&rows);
                    script.push_str("\\.\n");
                    data.push(rows);
                }
                PgPiece::Meta(line) => {
                    script.push('\n');
                    script.push_str(&line);
                    script.push('\n');
                    if !line.contains("restrict") {
                        expected.push(line);
                    }
                }
            }
        }
        let dialect = dialect("postgres");
        check_shape("postgres", dialect.as_ref(), &script)?;
        let spans = dialect.split_statements(&script);
        prop_assert_eq!(texts(&spans, &script), expected);
        let given: Vec<&str> = spans
            .iter()
            .filter_map(|s| dialect.copy_data(&script, s))
            .map(|r| &script[r])
            .collect();
        prop_assert_eq!(given, data);
        prop_assert!(left_out("postgres", dialect.as_ref(), &script).is_none());
    }
}

// ------------------------------------------------------------------------------------ MySQL

/// A piece of a MySQL script: a statement, one written only as executable comments (as
/// `mysqldump` writes views and triggers, SDB-113), or a routine under `DELIMITER`.
#[derive(Debug, Clone)]
enum MysqlPiece {
    Statement(String, &'static str),
    Executable(String, &'static str),
    Routine(String, &'static str),
}

fn mysql_piece() -> BoxedStrategy<MysqlPiece> {
    let executable = (
        prop::sample::select(vec!["/*!40101 ", "/*!50001 ", "/*M!100616 ", "/*!"]),
        statement_with("set", plain_atom(), 4),
    )
        .prop_map(|(open, body)| format!("{open}{body} */"));
    let routine = prop::collection::vec(statement_with("select", atom("mysql"), 5), 1..4)
        .prop_map(|inner| format!("create procedure p() begin {}; end", inner.join("; ")));
    prop_oneof![
        5 => (statement("mysql"), separator()).prop_map(|(s, sep)| MysqlPiece::Statement(s, sep)),
        2 => (executable, separator()).prop_map(|(s, sep)| MysqlPiece::Executable(s, sep)),
        2 => (routine, prop::sample::select(vec!["$$", "//", ";;"]))
            .prop_map(|(s, d)| MysqlPiece::Routine(s, d)),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(config(256))]

    /// MySQL scripts as dumps and the `mysql` client write them split back into their
    /// statements: executable comments alone are statements, `DELIMITER` lines are not.
    #[test]
    fn mysql_scripts_split_back(pieces in prop::collection::vec(mysql_piece(), 1..7)) {
        let mut script = String::new();
        let mut expected = Vec::new();
        for piece in pieces {
            match piece {
                MysqlPiece::Statement(text, sep) | MysqlPiece::Executable(text, sep) => {
                    script.push_str(&text);
                    script.push_str(sep);
                    expected.push(self::expected(&text));
                }
                MysqlPiece::Routine(text, delimiter) => {
                    script.push_str(&format!("\nDELIMITER {delimiter}\n{text}{delimiter}\nDELIMITER ;\n"));
                    expected.push(self::expected(&text));
                }
            }
        }
        for label in ["mysql", "mariadb"] {
            let dialect = dialect(label);
            check_shape(label, dialect.as_ref(), &script)?;
            let spans = dialect.split_statements(&script);
            prop_assert_eq!(texts(&spans, &script), expected.clone(), "{}", label);
        }
    }
}

// ------------------------------------------------------------------------------- SQL Server

/// A batch of a T-SQL script, between `GO` lines.
#[derive(Debug, Clone)]
enum Batch {
    /// Statements ended by `;`, some of them blocks, `BEGIN TRAN` or `MERGE`.
    Statements(Vec<(String, &'static str)>),
    /// A `DECLARE` or a `CREATE PROCEDURE` and the rest of the batch: one span (SDB-112).
    RestOfBatch(String),
}

fn tsql_statement() -> S {
    let inner = || statement_with("select", atom("sqlserver"), 5);
    prop_oneof![
        5 => inner(),
        2 => prop::collection::vec(inner(), 1..3)
            .prop_map(|s| format!("if x = 1 begin {}; end", s.join("; "))),
        1 => prop::sample::select(vec!["begin tran", "BEGIN TRANSACTION", "commit", "begin distributed transaction"])
            .prop_map(str::to_string),
        1 => inner().prop_map(|s| format!("merge t using s on t.id = s.id when matched then delete {}", &s[6..])),
    ]
    .boxed()
}

fn batch() -> BoxedStrategy<Batch> {
    let inner = || statement_with("select", atom("sqlserver"), 5);
    prop_oneof![
        3 => prop::collection::vec((tsql_statement(), separator()), 1..4).prop_map(Batch::Statements),
        1 => (prop::sample::select(vec!["declare @x int = 1", "DECLARE @t TABLE (a int)", "create procedure p as select 1", "CREATE OR ALTER VIEW v AS select 1"]),
              prop::collection::vec(inner(), 0..3))
            .prop_map(|(head, rest)| {
                let mut text = head.to_string();
                for s in rest {
                    text.push_str("; ");
                    text.push_str(&s);
                }
                Batch::RestOfBatch(text)
            }),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(config(256))]

    /// T-SQL scripts split back: `BEGIN … END` blocks whole, `BEGIN TRAN` alone, `MERGE` with its
    /// `;`, a `DECLARE` or a procedure with the rest of its batch, `GO` lines between batches.
    #[test]
    fn sqlserver_scripts_split_back(
        batches in prop::collection::vec(batch(), 1..5),
        go in prop::sample::select(vec!["\nGO\n", "\ngo\n", "\n  GO 2\n", "\nGO -- next\n", "\r\nGO\r\n"]),
    ) {
        let mut script = String::new();
        let mut expected = Vec::new();
        for (n, batch) in batches.into_iter().enumerate() {
            if n > 0 {
                script.push_str(go);
            }
            match batch {
                Batch::Statements(statements) => {
                    for (text, sep) in statements {
                        script.push_str(&text);
                        script.push_str(sep);
                        // MERGE keeps its `;`, and what is between them.
                        let kept = match (text.starts_with("merge"), sep.find(';')) {
                            (true, Some(at)) => &sep[..=at],
                            _ => "",
                        };
                        expected.push(self::expected(&format!("{text}{kept}")));
                    }
                }
                Batch::RestOfBatch(text) => {
                    script.push_str(&text);
                    expected.push(self::expected(&text));
                }
            }
        }
        let dialect = dialect("sqlserver");
        check_shape("sqlserver", dialect.as_ref(), &script)?;
        let spans = dialect.split_statements(&script);
        prop_assert_eq!(texts(&spans, &script), expected);
        prop_assert!(left_out("sqlserver", dialect.as_ref(), &script).is_none());
    }
}

// ----------------------------------------------------------------------------------- SQLite

fn sqlite_statement() -> S {
    let inner = || statement_with("select", atom("sqlite"), 5);
    let trigger = (
        prop::sample::select(vec![
            "create trigger tr after insert on t",
            "CREATE TEMP TRIGGER tr BEFORE DELETE ON t",
            "create temporary trigger tr after update on t for each row",
        ]),
        prop::collection::vec(
            prop_oneof![
                3 => inner(),
                1 => single_quoted().prop_map(|s| format!("update u set x = case when 1 then {s} end")),
            ],
            1..4,
        ),
    )
        .prop_map(|(head, body)| format!("{head} begin {}; end", body.join("; ")));
    prop_oneof![
        4 => inner(),
        2 => trigger,
        1 => prop::sample::select(vec!["begin transaction", "end", "BEGIN", "commit"]).prop_map(str::to_string),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(config(256))]

    /// SQLite scripts with triggers (`;` inside `BEGIN … END`, `CASE … END` inside them) and
    /// transaction words split back into their statements.
    #[test]
    fn sqlite_scripts_split_back((expected, script) in script_of(sqlite_statement())) {
        for label in ["sqlite", "libsql"] {
            let dialect = dialect(label);
            check_shape(label, dialect.as_ref(), &script)?;
            let spans = dialect.split_statements(&script);
            prop_assert_eq!(texts(&spans, &script), expected.clone(), "{}", label);
        }
    }
}

// ----------------------------------------------------------------------------------- Oracle

/// A piece of an Oracle script: plain SQL ended by `;` or a `/` line, or a PL/SQL unit, which
/// keeps its `;` and ends only at a `/` line.
#[derive(Debug, Clone)]
enum OraclePiece {
    Sql(String, &'static str),
    Unit(String),
}

fn oracle_piece() -> BoxedStrategy<OraclePiece> {
    let inner = || statement_with("select", atom("oracle"), 5);
    let unit = (
        prop::sample::select(vec![
            "begin",
            "declare v number; begin",
            "create or replace procedure p as begin",
            "CREATE FUNCTION f RETURN number AS BEGIN",
        ]),
        prop::collection::vec(inner(), 1..3),
    )
        .prop_map(|(head, body)| format!("{head} {}; end;", body.join("; ")));
    prop_oneof![
        4 => (inner(), prop::sample::select(vec![";", ";\n", "\n/\n", "\n  /  \n"]))
            .prop_map(|(s, sep)| OraclePiece::Sql(s, sep)),
        2 => unit.prop_map(OraclePiece::Unit),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(config(256))]

    /// Oracle scripts split back: plain SQL at `;` or `/` lines, PL/SQL units whole up to their
    /// `/` line.
    #[test]
    fn oracle_scripts_split_back(pieces in prop::collection::vec(oracle_piece(), 1..6)) {
        let mut script = String::new();
        let mut expected = Vec::new();
        for piece in pieces {
            match piece {
                OraclePiece::Sql(text, sep) => {
                    script.push_str(&text);
                    script.push_str(sep);
                    expected.push(self::expected(&text));
                }
                OraclePiece::Unit(text) => {
                    script.push_str(&text);
                    script.push_str("\n/\n");
                    expected.push(self::expected(&text));
                }
            }
        }
        let dialect = dialect("oracle");
        check_shape("oracle", dialect.as_ref(), &script)?;
        let spans = dialect.split_statements(&script);
        prop_assert_eq!(texts(&spans, &script), expected);
        prop_assert!(left_out("oracle", dialect.as_ref(), &script).is_none());
    }
}

// ------------------------------------------------------------------------------------- CQL

fn cql_statement() -> S {
    let inner = || statement_with("select", atom("cassandra"), 5);
    prop_oneof![
        3 => inner(),
        1 => prop::collection::vec(inner().prop_map(|s| format!("insert into t {}", &s[6..])), 1..4)
            .prop_map(|s| format!("begin batch {}; apply batch", s.join("; "))),
        1 => prop::collection::vec(inner(), 1..3)
            .prop_map(|s| format!("BEGIN UNLOGGED BATCH {}; APPLY BATCH", s.join("; "))),
    ]
    .boxed()
}

proptest! {
    #![proptest_config(config(256))]

    /// CQL scripts with batches split back: a batch is one statement.
    #[test]
    fn cql_scripts_split_back((expected, script) in script_of(cql_statement())) {
        let dialect = dialect("cassandra");
        check_shape("cassandra", dialect.as_ref(), &script)?;
        let spans = dialect.split_statements(&script);
        prop_assert_eq!(texts(&spans, &script), expected);
    }
}
