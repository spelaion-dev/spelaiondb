//! "Run from the cursor to the end" and the statement picker, on every engine's dialect: the
//! text the core gives must be read by that engine's splitter as exactly the statements
//! chosen, whatever separates them (a `;`, a line, a `GO`, a PL/SQL unit's `/`, MySQL's
//! `DELIMITER`). Only text: no database is needed.

use spelaion_config::Engine;
use spelaion_core::{PickedText, dialect_of, from_cursor, pick_statements, statement_at};
use spelaion_driver_api::Dialect;

/// A script in a dialect, with `|` where the cursor is and how many statements come before
/// the one under it.
struct Case {
    script: &'static str,
    before: usize,
    /// Text of a statement that must not run, and of the first one that must.
    skipped: &'static str,
    first: &'static str,
}

const SQL: Case = Case {
    script: "select 1;\nselect 'a;|b';\n-- c\nselect 3;\n\nselect 4",
    before: 1,
    skipped: "select 1",
    first: "select 'a;b'",
};

/// The scripts each dialect is tried with, by the dialect's name. A new dialect has none
/// until it is given its own here.
fn cases(dialect: &str) -> Vec<Case> {
    match dialect {
        "sqlite" | "duckdb" | "clickhouse" | "trino" | "databend" | "spanner" | "tdengine"
        | "flightsql" | "dremio" | "gizmosql" | "h2" | "hsqldb" | "hive" | "spark" | "kyuubi"
        | "impala" => {
            vec![SQL]
        }
        "postgres" => vec![
            SQL,
            // A `;` inside a dollar-quoted body before the cursor.
            Case {
                script: "do $$ begin perform 1; end $$;\nselect |2;\nselect 3",
                before: 1,
                skipped: "perform 1",
                first: "select 2",
            },
        ],
        "mysql" => vec![
            SQL,
            // Under `DELIMITER $$` the `;` inside a body does not end its statement: the
            // text must keep the `DELIMITER` line that comes before the cursor.
            Case {
                script: "DELIMITER $$\ncreate procedure p() begin select 1; select 2; end$$\n\
                         create procedure q() begin select 3; sel|ect 4; end$$\nDELIMITER ;\n\
                         call p();\nselect 5",
                before: 1,
                skipped: "procedure p",
                first: "create procedure q() begin select 3; select 4; end",
            },
        ],
        "sqlserver" => vec![
            SQL,
            // A `GO` batch: its `DECLARE` keeps the rest of the batch with it.
            Case {
                script: "select 1\nGO\ndeclare @x int = 2;\nselect |@x;\nGO\nselect 3\nGO\n",
                before: 1,
                skipped: "select 1",
                first: "declare @x int = 2;\nselect @x;",
            },
            Case {
                script: "print 'a'\nGO\n\n|\nselect 2\ngo\nselect 3",
                before: 1,
                skipped: "print",
                first: "select 2",
            },
        ],
        "oracle" => vec![
            Case {
                script: "select 1 from dual;\nselect 'a;|b' from dual;\nselect 3 from dual",
                before: 1,
                skipped: "select 1",
                first: "select 'a;b' from dual",
            },
            // A PL/SQL unit keeps its `;`s until the line that holds only `/`.
            Case {
                script: "select 1 from dual;\ncreate or replace procedure p as\nbegin\n  \
                         nu|ll;\n  dbms_output.put_line('a;b');\nend;\n/\nselect 2 from dual;\n\
                         begin\n  null;\nend;\n/\n",
                before: 1,
                skipped: "select 1",
                first: "create or replace procedure p as\nbegin\n  null;\n  \
                        dbms_output.put_line('a;b');\nend;",
            },
            // From a statement after a unit: the unit's `/` stays behind.
            Case {
                script: "begin\n  null;\nend;\n/\nselect |2 from dual;\nselect 3 from dual;",
                before: 1,
                skipped: "null",
                first: "select 2 from dual",
            },
        ],
        "cassandra" => vec![
            SQL,
            Case {
                script: "SELECT * FROM t;\nBEGIN BATCH\n  INSERT INTO t (a) VALUES (1);\n  \
                         UPDATE t SET b = |2 WHERE a = 1;\nAPPLY BATCH;\nSELECT a FROM t;",
                before: 1,
                skipped: "SELECT *",
                first: "BEGIN BATCH\n  INSERT INTO t (a) VALUES (1);\n  UPDATE t SET b = 2 \
                        WHERE a = 1;\nAPPLY BATCH",
            },
        ],
        "mongodb" => vec![Case {
            script: "db.a.find()\n// c; d\ndb.b.find({x: ';|'});db.c\n  .find()\n  .limit(2)\n\n\
                     db.d.insertOne({\n  a: 1\n})\n",
            before: 1,
            skipped: "db.a",
            first: "db.b.find({x: ';'})",
        }],
        "redis" => vec![Case {
            script: "SET a 1\r\n\n   GET| a   \n#GET b\nDEL a",
            before: 1,
            skipped: "SET",
            first: "GET a",
        }],
        "neo4j" => vec![Case {
            script: "MATCH (n) RETURN n;\nMATCH (m) RETURN 'a;|b';\n// c\nRETURN 3",
            before: 1,
            skipped: "(n)",
            first: "MATCH (m) RETURN 'a;b'",
        }],
        "elasticsearch" | "opensearch" | "solr" | "meilisearch" => vec![Case {
            script: "# Requests\nGET /one\n\nGET /a/_search\n{\n\"query\": {\"match\": \
                     {\"text\": \"DELETE /x|\"}}\n}\n\nDELETE /b\n",
            before: 1,
            skipped: "/one",
            first: "GET /a/_search\n{\n\"query\": {\"match\": {\"text\": \"DELETE /x\"}}\n}",
        }],
        other => panic!("no script for the dialect {other}: give it one in this test"),
    }
}

fn texts<'a>(dialect: &dyn Dialect, script: &'a str) -> Vec<&'a str> {
    dialect
        .split_statements(script)
        .iter()
        .map(|s| s.text(script))
        .collect()
}

fn utf16(text: &str) -> usize {
    text.encode_utf16().count()
}

/// `picked` runs exactly `wanted`, each statement where the editor shows it.
fn assert_runs(
    dialect: &dyn Dialect,
    script: &str,
    picked: &PickedText,
    wanted: &[&str],
    label: &str,
) {
    assert_eq!(texts(dialect, &picked.text), wanted, "{label}");
    assert_eq!(picked.statements, wanted.len(), "{label}");
    let covered = &script[picked.start..picked.end];
    assert_eq!(utf16(&picked.text), utf16(covered), "{label}");
    // A position in the text is a position in the script, as the editor counts them.
    for span in dialect.split_statements(&picked.text) {
        let at = utf16(&script[..picked.start]) + utf16(&picked.text[..span.start]);
        let units: Vec<u16> = script
            .encode_utf16()
            .skip(at)
            .take(utf16(span.text(&picked.text)))
            .collect();
        assert_eq!(
            String::from_utf16(&units).unwrap(),
            span.text(&picked.text),
            "{label}"
        );
    }
}

#[test]
fn from_the_cursor_to_the_end_on_every_dialect() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in Engine::ALL {
        let dialect = dialect_of(engine).unwrap_or_else(|| panic!("{engine:?} has no dialect"));
        for (number, case) in cases(dialect.name()).iter().enumerate() {
            let label = format!("{} #{number}", engine.as_str());
            let offset = case.script.find('|').expect("cursor");
            let script = case.script.replacen('|', "", 1);
            let all = texts(dialect.as_ref(), &script);
            assert!(all.len() > case.before + 1, "{label}: {all:?}");
            assert_eq!(all[case.before], case.first, "{label}");
            assert_eq!(
                statement_at(dialect.as_ref(), &script, offset).map(|s| s.text(&script)),
                Some(case.first),
                "{label}"
            );
            let picked = from_cursor(dialect.as_ref(), &script, offset)
                .unwrap_or_else(|e| panic!("{label}: {e}"))
                .unwrap_or_else(|| panic!("{label}: nothing to run"));
            assert_runs(
                dialect.as_ref(),
                &script,
                &picked,
                &all[case.before..],
                &label,
            );
            assert!(
                !picked.text.contains(case.skipped),
                "{label}: {:?}",
                picked.text
            );
            // From the first statement: the script's statements, all of them.
            let whole = from_cursor(dialect.as_ref(), &script, 0).unwrap().unwrap();
            assert_runs(dialect.as_ref(), &script, &whole, &all, &label);
        }
    }
}

#[test]
fn the_picker_runs_only_the_statements_chosen_on_every_dialect() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in Engine::ALL {
        let dialect = dialect_of(engine).unwrap_or_else(|| panic!("{engine:?} has no dialect"));
        for (number, case) in cases(dialect.name()).iter().enumerate() {
            let script = case.script.replacen('|', "", 1);
            let all = texts(dialect.as_ref(), &script);
            // Every statement alone, every pair, and all but each one.
            let mut choices: Vec<Vec<usize>> = (0..all.len()).map(|i| vec![i]).collect();
            for a in 0..all.len() {
                for b in a + 1..all.len() {
                    choices.push(vec![a, b]);
                }
                choices.push((0..all.len()).filter(|i| *i != a).collect());
            }
            for choice in choices {
                let label = format!("{} #{number} {choice:?}", engine.as_str());
                let wanted: Vec<&str> = choice.iter().map(|i| all[*i]).collect();
                let picked = pick_statements(dialect.as_ref(), &script, &choice)
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_runs(dialect.as_ref(), &script, &picked, &wanted, &label);
            }
        }
    }
}

#[test]
fn a_statement_under_another_delimiter_is_not_run_as_its_pieces() {
    let _turn = crate::serial::file_turn(module_path!());
    // The body's `;` would split it under the default delimiter: the text keeps the
    // `DELIMITER` lines, and the statement before it is written over.
    let dialect = dialect_of(Engine::Mysql).unwrap();
    let script = "select 0;\nDELIMITER $$\ncreate procedure p() begin select 1; select 2; end$$\nDELIMITER ;\nselect 3";
    let picked = pick_statements(dialect.as_ref(), script, &[1]).unwrap();
    assert_eq!(
        texts(dialect.as_ref(), &picked.text),
        ["create procedure p() begin select 1; select 2; end"]
    );
    assert!(picked.text.contains("DELIMITER $$"));
    assert!(!picked.text.contains("select 0"));
    assert!(!picked.text.contains("select 3"));
}

/// Ctrl+Enter on a statement under `DELIMITER $$` runs it whole (SDB-403): its text is the
/// script with the other statements written over, so the run reads it with that delimiter. A
/// plain statement is sent alone, at its own place in the script.
#[test]
fn ctrl_enter_runs_a_statement_under_another_delimiter_whole() {
    let _turn = crate::serial::file_turn(module_path!());
    let dialect = dialect_of(Engine::Mysql).unwrap();
    let script = "select 0;\nDELIMITER $$\ncreate procedure p() begin select 1; select 2; end$$\nDELIMITER ;\nselect 3";
    let inside = script.find("select 2").unwrap();
    let (picked, statement) = spelaion_core::statement_to_run(dialect.as_ref(), script, inside)
        .unwrap()
        .expect("a statement");
    assert_eq!(
        &script[statement.start..statement.end],
        "create procedure p() begin select 1; select 2; end"
    );
    assert_eq!(
        texts(dialect.as_ref(), &picked.text),
        ["create procedure p() begin select 1; select 2; end"]
    );
    // Positions in the text are positions in the script: an error is underlined in place.
    assert_eq!(
        &picked.text[statement.start - picked.start..statement.end - picked.start],
        &script[statement.start..statement.end]
    );

    let last = script.rfind("select 3").unwrap();
    let (picked, statement) = spelaion_core::statement_to_run(dialect.as_ref(), script, last)
        .unwrap()
        .expect("a statement");
    assert_eq!(picked.text, "select 3");
    assert_eq!((picked.start, statement.start), (last, last));

    assert!(
        spelaion_core::statement_to_run(dialect.as_ref(), "-- nothing", 0)
            .unwrap()
            .is_none()
    );
}

/// On every engine, Ctrl+Enter's text is read by that engine's splitter as exactly the
/// statement under the cursor.
#[test]
fn ctrl_enter_text_is_the_statement_under_the_cursor_on_every_engine() {
    let _turn = crate::serial::file_turn(module_path!());
    for engine in Engine::ALL {
        let Some(dialect) = dialect_of(engine) else {
            continue;
        };
        let separator = match dialect.name() {
            "redis" => "\n",
            _ => ";\n",
        };
        let script = ["select 1", "select 2", "select 3"].join(separator);
        let offset = script.find("select 2").unwrap() + 2;
        let Ok(Some((picked, statement))) =
            spelaion_core::statement_to_run(dialect.as_ref(), &script, offset)
        else {
            continue;
        };
        assert_eq!(
            texts(dialect.as_ref(), &picked.text),
            [&script[statement.start..statement.end]],
            "{engine:?}"
        );
    }
}
