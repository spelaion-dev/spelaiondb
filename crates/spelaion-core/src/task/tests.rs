//! A task's document, its checks and the rule that it never holds a secret (ADR-089 points 2
//! and 3). Runs against databases are in `apps/cli/tests/task.rs`.

use std::path::{Path, PathBuf};

use spelaion_driver_api::Secret;
use spelaion_store::Store;

use super::*;
use crate::export::{CsvOptions, ExportFormat, JsonOptions, SqlOptions, TextEncoding};
use crate::export_stream::FileNamePattern;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sdb-task-unit-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn absolute(dir: &Path) -> PathBuf {
    dir.join("out")
}

fn export_document(folder: PathBuf, sql: &str) -> TaskDocument {
    TaskDocument::new(
        TaskConnection {
            folder: None,
            id: "~/shop".into(),
            open_folders: Vec::new(),
        },
        TaskRequest::Export(ExportTask {
            source: ExportSource::Query { sql: sql.into() },
            settings: ExportSettings {
                format: ExportFormat::Csv,
                header: true,
                csv: CsvOptions {
                    delimiter: ';',
                    null_text: String::new(),
                    escape_formulas: true,
                },
                encoding: TextEncoding::Utf8,
                gzip: false,
                sql: SqlOptions::default(),
                json: JsonOptions::default(),
                columns: None,
            },
            folder,
            names: FileNamePattern {
                pattern: "numbers-{date}".into(),
                timestamp_format: "%Y%m%d-%H%M%S".into(),
            },
        }),
    )
}

fn dump_document(folder: PathBuf, extra_args: &str) -> TaskDocument {
    TaskDocument::new(
        TaskConnection {
            folder: None,
            id: "~/shop".into(),
            open_folders: Vec::new(),
        },
        TaskRequest::Dump(TaskDumpRequest {
            tools: TaskTools::Detect,
            folder,
            name_pattern: "{database}-{date}".into(),
            extra_args: extra_args.into(),
            options: crate::backup::DumpOptions::Postgres(crate::backup::PgDumpOptions::default()),
        }),
    )
}

#[test]
fn a_document_of_version_one_reads_as_it_was_written() {
    // Pinned: a task saved by this version must read in every later one. A change to the
    // core's option types that breaks this needs a new version and a reader for the old one.
    let saved = r#"{"version":1,"connection":{"folder":null,"id":"~/shop","openFolders":[]},"request":{"kind":"export","source":{"query":{"sql":"select 1"}},"settings":{"format":"csv","header":true,"csv":{"delimiter":";","nullText":"","escapeFormulas":true},"encoding":"utf8","gzip":false,"sql":{"rowsPerInsert":1,"omitSchema":false,"keywordCase":"upper","nameCase":"asIs","upsert":false},"json":{"isoDates":false,"jsonAsJson":true},"columns":null},"folder":"FOLDER","names":{"pattern":"numbers-{date}","timestampFormat":"%Y%m%d-%H%M%S"}}}"#;
    let folder = absolute(&std::env::temp_dir());
    let folder_json = serde_json::to_string(&folder).unwrap();
    let saved = saved.replace("\"FOLDER\"", &folder_json);
    let read = TaskDocument::from_json(&saved).unwrap();
    assert_eq!(read, export_document(folder, "select 1"));
    assert_eq!(read.to_json().unwrap(), saved);
    // Fields a later version adds are read at their defaults where the type has one.
    let older = saved.replace(r#","header":true"#, "");
    assert!(!match TaskDocument::from_json(&older).unwrap().request {
        TaskRequest::Export(export) => export.settings.header,
        other => panic!("{other:?}"),
    });
}

#[test]
fn a_newer_or_damaged_document_runs_nothing() {
    let folder = absolute(&std::env::temp_dir());
    let json = export_document(folder, "select 1").to_json().unwrap();
    let newer = json.replace(r#""version":1"#, r#""version":2"#);
    let reason = TaskDocument::from_json(&newer).unwrap_err();
    assert!(reason.contains("newer SpelaionDB"), "{reason}");
    assert!(TaskDocument::from_json("{").is_err());
    assert!(TaskDocument::from_json(r#"{"version":1}"#).is_err());
    // A document that would fail its checks reads as unreadable, never in part.
    let relative = json.replace(
        &serde_json::to_string(&absolute(&std::env::temp_dir())).unwrap(),
        "\"out\"",
    );
    assert!(TaskDocument::from_json(&relative).is_err());
    // A task never names a program to run as a dump's tools.
    let launcher = dump_document(absolute(&std::env::temp_dir()), "")
        .to_json()
        .unwrap()
        .replace(
            r#""tools":"detect""#,
            r#""tools":{"launcher":{"program":"/bin/sh","leadingArgs":[]}}"#,
        );
    assert!(TaskDocument::from_json(&launcher).is_err());
}

#[test]
fn the_checks_of_a_run_by_hand_apply_when_saved() {
    let folder = absolute(&std::env::temp_dir());
    let ok = export_document(folder.clone(), "select 1");
    ok.check().unwrap();
    let mut relative = ok.clone();
    if let TaskRequest::Export(export) = &mut relative.request {
        export.folder = PathBuf::from("out");
    }
    assert!(relative.check().is_err());
    let mut clipboard = ok.clone();
    if let TaskRequest::Export(export) = &mut clipboard.request {
        export.settings.format = ExportFormat::InList;
    }
    assert!(clipboard.check().is_err());
    let mut upsert = ok.clone();
    if let TaskRequest::Export(export) = &mut upsert.request {
        export.settings.sql.upsert = true;
    }
    assert!(upsert.check().is_err());
    let mut pattern = ok.clone();
    if let TaskRequest::Export(export) = &mut pattern.request {
        export.names.pattern = "{nope}".into();
    }
    assert!(pattern.check().is_err());
    assert!(export_document(folder.clone(), "  ").check().is_err());
    // A repository connection names its folder; a personal one does not.
    let mut mixed = ok.clone();
    mixed.connection.folder = Some(folder.clone());
    assert!(mixed.check().is_err());
    let mut repository = ok.clone();
    repository.connection = TaskConnection {
        folder: Some(folder.clone()),
        id: "prod/orders".into(),
        open_folders: vec![folder.clone()],
    };
    assert!(
        repository.check().is_err(),
        "only a personal one keeps folders"
    );
    repository.connection.open_folders.clear();
    repository.check().unwrap();
    assert!(dump_document(folder, "--schema 'unclosed").check().is_err());
    assert!(check_name("  ").is_err());
    assert!(check_name("two\nlines").is_err());
    assert_eq!(check_name("  Weekly  ").unwrap(), "Weekly");
    assert_eq!(
        check_folder(" Exports / / Sales ").unwrap(),
        "Exports/Sales"
    );
}

/// A personal connection whose password the configuration resolves by running a command, so
/// the test knows the secret without writing it into a file or the environment.
fn connection_with_secret(dir: &Path, secret: &str) -> ResolvedConnection {
    std::fs::create_dir_all(dir.join("connections")).unwrap();
    std::fs::write(
        dir.join("connections/shop.toml"),
        format!(
            "schema_version = 1\nname = 'Shop'\nengine = 'postgres'\nhost = 'db.invalid'\nport = 5432\nuser = 'reporter'\ndatabase = 'shop'\npassword = {{ secret = 'cmd:echo {secret}' }}\n"
        ),
    )
    .unwrap();
    let (resolved, _) = resolve_connection(
        dir,
        &TaskConnection {
            folder: None,
            id: "~/shop".into(),
            open_folders: Vec::new(),
        },
    )
    .unwrap();
    resolved
}

#[test]
fn no_secret_is_ever_written_into_a_task() {
    let dir = temp("secrets");
    let secret = "t0p-s3cret-pw";
    let resolved = connection_with_secret(&dir, secret);
    assert!(
        matches!(&resolved.password, spelaion_config::Password::Value(v) if v.expose() == secret),
        "the test resolves its secret"
    );
    let typed = Secret::new("typed-pass-9");
    let secrets = secrets_of(&resolved, Some(&typed));
    let store = Store::open(&dir.join("state.db")).unwrap();
    let out = absolute(&dir);
    let refused = [
        export_document(out.clone(), &format!("select '{secret}' as leaked")),
        export_document(out.join(secret), "select 1"),
        export_document(out.clone(), "select 'typed-pass-9'"),
        dump_document(out.clone(), &format!("--no-owner --password={secret}")),
        // A secret-named key=value in a tool's arguments, even one the test does not know.
        dump_document(out.clone(), "--set=PGPASSWORD=whatever"),
    ];
    for document in &refused {
        match save_task(&store, None, "Leaky", "", document, &secrets) {
            Err(TaskError::Secret(_)) => {}
            other => panic!("saved a task holding a secret: {other:?}"),
        }
    }
    assert!(store.tasks().unwrap().is_empty(), "nothing was written");
    // A password that is also the user or database name is plain text already.
    let ok = export_document(out.clone(), "select * from shop.reporter_totals");
    let id = save_task(&store, None, "Totals", "Reports", &ok, &secrets).unwrap();
    let dump = dump_document(out, "--no-owner");
    save_task(&store, None, "Dump", "", &dump, &secrets).unwrap();
    for task in store.tasks().unwrap() {
        for text in [&task.name, &task.folder, &task.document] {
            assert!(!text.contains(secret) && !text.contains(typed.expose()));
        }
    }
    let read = task(&store, id).unwrap();
    assert_eq!(read.document.unwrap(), ok);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn tasks_are_renamed_duplicated_and_listed_with_unreadable_ones() {
    let dir = temp("list");
    let store = Store::open(&dir.join("state.db")).unwrap();
    let secrets = Redactor::new();
    let document = export_document(absolute(&dir), "select 1");
    let id = save_task(&store, None, "Weekly", "Exports", &document, &secrets).unwrap();
    assert!(matches!(
        save_task(&store, None, "WEEKLY", "", &document, &secrets),
        Err(TaskError::NameTaken(_))
    ));
    let copy = duplicate_task(&store, id).unwrap();
    let again = duplicate_task(&store, id).unwrap();
    assert_eq!(task(&store, copy).unwrap().name, "Weekly (copy)");
    assert_eq!(task(&store, again).unwrap().name, "Weekly (copy 2)");
    rename_task(&store, again, "Monthly", " Exports / Old ").unwrap();
    assert_eq!(task(&store, again).unwrap().folder, "Exports/Old");
    assert!(matches!(
        rename_task(&store, again, "weekly", ""),
        Err(TaskError::NameTaken(_))
    ));
    // A document damaged on disk is listed as unreadable, and runs nothing.
    store
        .save_task(Some(copy), "Weekly (copy)", "Exports", "{\"version\":1}")
        .unwrap();
    let listed = list_tasks(&store).unwrap();
    assert_eq!(listed.len(), 3);
    let damaged = listed.iter().find(|t| t.id == copy).unwrap();
    assert!(damaged.document.is_err());
    assert_eq!(task_named(&store, "monthly").unwrap().id, again);
    assert!(matches!(
        task_named(&store, "nope"),
        Err(TaskError::NotFound(_))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}
