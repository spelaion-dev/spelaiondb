//! Saved tasks end to end (ADR-089) with a real PostgreSQL: a task saved as the app saves it
//! runs from the core as the app's Tasks view starts it and from the real `spelaiondb` binary,
//! writes the same file both times, and its history shows both runs. Without
//! `SPELAION_TEST_PG` these tests are skipped, which does not count as tested.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use spelaion_core::task::{
    self, ExportSettings, ExportSource, ExportTask, Questions, RunFileTask, TaskConnection,
    TaskDocument, TaskEnv, TaskOutcome, TaskRequest, TaskRunState,
};
use spelaion_core::{CsvOptions, ExportFormat, JsonOptions, SqlOptions, TextEncoding};
use spelaion_core::{Engines, FileNamePattern, JobCancel, TunnelPool};
use spelaion_store::{Store, TaskOrigin};

const BIN: &str = env!("CARGO_BIN_EXE_spelaiondb");

struct Fixture {
    base: PathBuf,
}

impl Fixture {
    /// A personal folder with a PostgreSQL connection whose password the configuration
    /// resolves by running a command (so no process environment is changed), or asks for.
    fn new(name: &str, address: &str) -> Self {
        let base = std::env::temp_dir().join(format!("sdb-task-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("user/connections")).unwrap();
        let (host, port) = address.rsplit_once(':').unwrap();
        let connection = |name: &str, password: &str| {
            format!(
                "schema_version = 1\nname = '{name}'\nengine = 'postgres'\nhost = '{host}'\nport = {port}\nuser = 'spelaion'\ndatabase = 'spelaion'\npassword = {{ secret = '{password}' }}\n"
            )
        };
        std::fs::write(
            base.join("user/connections/numbers.toml"),
            connection("Numbers", "cmd:echo spelaion"),
        )
        .unwrap();
        std::fs::write(
            base.join("user/connections/asks.toml"),
            connection("Asks", "prompt"),
        )
        .unwrap();
        Self { base }
    }

    fn user(&self) -> PathBuf {
        self.base.join("user")
    }

    fn out(&self) -> PathBuf {
        self.base.join("out")
    }

    fn store(&self) -> Store {
        Store::open(&spelaion_config::state_db(&self.user())).unwrap()
    }

    /// Saves a task as the app's "Save as task…" does: checked, with the connection's
    /// resolved secrets as the judge of what it may hold.
    fn save(&self, name: &str, connection: &str, request: TaskRequest) -> i64 {
        let connection = TaskConnection {
            folder: None,
            id: connection.into(),
            open_folders: Vec::new(),
        };
        let (resolved, _) = task::resolve_connection(&self.user(), &connection).unwrap();
        let document = TaskDocument::new(connection, request);
        task::save_task(
            &self.store(),
            None,
            name,
            "Exports",
            &document,
            &task::secrets_of(&resolved, None),
        )
        .unwrap()
    }

    /// The app's environment for a run: its tunnels, its dialogs (none answered here) and the
    /// password its dialog asked for, if any.
    fn app(&self, password: Option<&str>) -> TaskEnv {
        TaskEnv {
            user_dir: self.user(),
            engines: Arc::new(Engines::builtin()),
            tunnels: Arc::new(TunnelPool::new()),
            interaction: Arc::new(spelaion_ssh::NonInteractive),
            password: password.map(spelaion_driver_api::Secret::new),
            questions: Questions::Ask,
            tools: None,
        }
    }

    fn cli(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("SPELAION_USER_DIR", self.user())
            .output()
            .unwrap()
    }

    fn cli_ok(&self, args: &[&str]) -> String {
        let out = self.cli(args);
        assert!(
            out.status.success(),
            "spelaiondb {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn export_numbers(folder: &Path) -> TaskRequest {
    TaskRequest::Export(ExportTask {
        source: ExportSource::Query {
            sql: "select n, 'row ' || n as label from generate_series(1, 2500) as n order by n"
                .into(),
        },
        settings: ExportSettings {
            format: ExportFormat::Csv,
            header: true,
            csv: CsvOptions::default(),
            encoding: TextEncoding::Utf8,
            gzip: false,
            sql: SqlOptions::default(),
            json: JsonOptions::default(),
            columns: None,
        },
        folder: folder.to_path_buf(),
        names: FileNamePattern {
            pattern: "numbers".into(),
            ..FileNamePattern::default()
        },
    })
}

async fn run_from_app(id: i64, env: &TaskEnv, confirmed: bool) -> TaskOutcome {
    task::run_task(
        id,
        TaskOrigin::App,
        env,
        confirmed,
        &JobCancel::new(),
        &|_| {},
    )
    .await
    .unwrap()
}

fn test_pg() -> Option<String> {
    let address = std::env::var("SPELAION_TEST_PG").ok();
    if address.is_none() {
        eprintln!("SPELAION_TEST_PG not set: saved tasks not tested");
    }
    address
}

#[tokio::test(flavor = "multi_thread")]
async fn an_export_task_runs_the_same_from_the_app_and_the_cli_and_its_history_shows_both() {
    let Some(address) = test_pg() else { return };
    let fx = Fixture::new("export", &address);
    let id = fx.save("Weekly numbers", "~/numbers", export_numbers(&fx.out()));

    // From the app: the run the Tasks view's Run starts.
    match run_from_app(id, &fx.app(None), false).await {
        TaskOutcome::Ran {
            state: TaskRunState::Succeeded,
            summary,
            ..
        } => assert!(summary.starts_with("2500 rows"), "{summary}"),
        other => panic!("the app's run: {other:?}"),
    }
    // From the command line, by name in any case, as a scheduler would call it.
    let printed = fx.cli_ok(&["task", "run", "weekly NUMBERS"]);
    assert!(printed.starts_with("2500 rows"), "{printed}");

    // The same file twice, the second never over the first.
    let first = std::fs::read_to_string(fx.out().join("numbers.csv")).unwrap();
    let second = std::fs::read_to_string(fx.out().join("numbers (2).csv")).unwrap();
    assert_eq!(first, second);
    assert_eq!(first.lines().count(), 2501);
    assert_eq!(first.lines().next(), Some("n,label"));

    // Its history shows both runs, newest first, from the store and from the CLI.
    let runs = task::task_runs(&fx.store(), id, 10).unwrap();
    let seen: Vec<_> = runs.iter().map(|r| (r.origin, r.state)).collect();
    assert_eq!(
        seen,
        [
            (TaskOrigin::Cli, TaskRunState::Succeeded),
            (TaskOrigin::App, TaskRunState::Succeeded)
        ]
    );
    let history = fx.cli_ok(&["task", "history", "Weekly numbers", "--log"]);
    let entries: Vec<&str> = history.lines().filter(|l| l.starts_with('[')).collect();
    assert_eq!(entries.len(), 2, "{history}");
    assert!(entries[0].contains("cli · succeeded"), "{history}");
    assert!(entries[1].contains("app · succeeded"), "{history}");
    assert!(
        history.contains("started from the command line"),
        "{history}"
    );
    assert!(history.contains("started from the app"), "{history}");
    let listed = fx.cli_ok(&["task", "list"]);
    assert!(
        listed.contains("Exports/Weekly numbers — export on ~/numbers — last run"),
        "{listed}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_task_that_asks_first_needs_a_yes_from_the_app_and_from_the_cli() {
    let Some(address) = test_pg() else { return };
    let fx = Fixture::new("asks", &address);
    std::fs::create_dir_all(fx.out()).unwrap();
    let file = fx.out().join("setup.sql");
    std::fs::write(
        &file,
        "create temporary table sdb_task_cli (n int);\ninsert into sdb_task_cli values (1);\n",
    )
    .unwrap();
    let id = fx.save(
        "Setup",
        "~/numbers",
        TaskRequest::RunFile(RunFileTask {
            file: file.clone(),
            on_error: Default::default(),
        }),
    );
    // The app asks, and records nothing until the run is confirmed.
    let env = fx.app(None);
    match run_from_app(id, &env, false).await {
        TaskOutcome::NeedsConfirmation(plan) => {
            assert!(plan.needs_confirmation);
            assert!(plan.steps[0].starts_with("2 statements"), "{plan:?}");
        }
        other => panic!("{other:?}"),
    }
    assert!(task::task_runs(&fx.store(), id, 10).unwrap().is_empty());
    assert!(matches!(
        run_from_app(id, &env, true).await,
        TaskOutcome::Ran {
            state: TaskRunState::Succeeded,
            ..
        }
    ));
    // The command line refuses it without --yes, recorded, and runs it with it.
    let refused = fx.cli(&["task", "run", "Setup"]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--yes"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    fx.cli_ok(&["task", "run", "Setup", "--yes"]);
    let states: Vec<_> = task::task_runs(&fx.store(), id, 10)
        .unwrap()
        .into_iter()
        .map(|r| (r.origin, r.state))
        .collect();
    assert_eq!(
        states,
        [
            (TaskOrigin::Cli, TaskRunState::Succeeded),
            (TaskOrigin::Cli, TaskRunState::Refused),
            (TaskOrigin::App, TaskRunState::Succeeded)
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_password_the_connection_asks_for_is_asked_in_the_app_and_never_in_the_cli() {
    let Some(address) = test_pg() else { return };
    let fx = Fixture::new("password", &address);
    let id = fx.save("Asks", "~/asks", export_numbers(&fx.out()));
    // The app asks for it, records nothing, and runs with the password its dialog got.
    assert!(matches!(
        run_from_app(id, &fx.app(None), false).await,
        TaskOutcome::PasswordRequired { .. }
    ));
    assert!(task::task_runs(&fx.store(), id, 10).unwrap().is_empty());
    assert!(matches!(
        run_from_app(id, &fx.app(Some("spelaion")), false).await,
        TaskOutcome::Ran {
            state: TaskRunState::Succeeded,
            ..
        }
    ));
    // The command line asks nobody: the run fails, recorded, saying how to run it unattended.
    let failed = fx.cli(&["task", "run", "Asks"]);
    assert!(!failed.status.success());
    let said = String::from_utf8_lossy(&failed.stderr);
    assert!(said.contains("keychain"), "{said}");
    let runs = task::task_runs(&fx.store(), id, 10).unwrap();
    assert_eq!(runs[0].origin, TaskOrigin::Cli);
    assert_eq!(runs[0].state, TaskRunState::Failed);
    assert!(runs[0].summary.contains("keychain"), "{}", runs[0].summary);
}

#[test]
fn an_unknown_task_is_said_and_fails() {
    let base = std::env::temp_dir().join(format!("sdb-task-cli-unknown-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let out = Command::new(BIN)
        .args(["task", "run", "nope"])
        .env("SPELAION_USER_DIR", &base)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("there is no task named \"nope\""));
    let listed = Command::new(BIN)
        .args(["task", "list"])
        .env("SPELAION_USER_DIR", &base)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&listed.stdout).contains("No saved tasks"));
    let _ = std::fs::remove_dir_all(&base);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_import_task_that_replaces_its_table_asks_and_loads_the_file_as_it_is_then() {
    let Some(address) = test_pg() else { return };
    let fx = Fixture::new("import", &address);
    std::fs::create_dir_all(fx.out()).unwrap();
    let file = fx.out().join("prices.csv");
    std::fs::write(&file, "sku,price\nA,1.5\nB,2\n").unwrap();
    let table = format!("sdb_task_import_{}", std::process::id());
    let id = fx.save(
        "Prices",
        "~/numbers",
        TaskRequest::Import(spelaion_core::task::ImportTask {
            file: file.clone(),
            options: spelaion_core::ParseOptions {
                header: true,
                ..Default::default()
            },
            target: spelaion_core::ImportTarget::New {
                schema: "public".into(),
                table: table.clone(),
                if_exists: spelaion_core::IfExists::Replace,
            },
            columns: vec![
                spelaion_core::ColumnMapping::New {
                    name: "sku".into(),
                    sql_type: "text".into(),
                },
                spelaion_core::ColumnMapping::New {
                    name: "price".into(),
                    sql_type: "numeric".into(),
                },
            ],
            commit_every: 0,
        }),
    );
    // The first run creates the table: nothing to ask, as by hand.
    let env = fx.app(None);
    match run_from_app(id, &env, false).await {
        TaskOutcome::Ran {
            state: TaskRunState::Succeeded,
            summary,
            ..
        } => assert!(summary.ends_with("(created)"), "{summary}"),
        other => panic!("{other:?}"),
    }
    // Now replacing it is a DROP: the guard asks, so the app asks with the plan.
    match run_from_app(id, &env, false).await {
        TaskOutcome::NeedsConfirmation(plan) => {
            assert!(
                plan.steps.iter().any(|s| s.starts_with("DROP TABLE")),
                "{plan:?}"
            );
            assert!(!plan.risks.is_empty());
        }
        other => panic!("{other:?}"),
    }
    assert!(!fx.cli(&["task", "run", "Prices"]).status.success());
    // The file is read again when the task runs, as it is then.
    std::fs::write(&file, "sku,price\nA,1.5\nB,2\nC,3\n").unwrap();
    let printed = fx.cli_ok(&["task", "run", "Prices", "--yes"]);
    assert!(printed.starts_with("3 rows into public."), "{printed}");
    // Clean up through a task too: a file run on the same connection.
    let drop = fx.out().join("drop.sql");
    std::fs::write(&drop, format!("drop table {table};\n")).unwrap();
    fx.save(
        "Drop",
        "~/numbers",
        TaskRequest::RunFile(RunFileTask {
            file: drop,
            on_error: Default::default(),
        }),
    );
    fx.cli_ok(&["task", "run", "Drop", "--yes"]);
}

/// The vendor's tool: a local folder in `SPELAION_TEST_DB_TOOLS`, or the image's own with
/// `docker run`, as `spelaion-core`'s backup tests use it.
fn dump_tools() -> spelaion_core::backup::ToolSource {
    if let Ok(folder) = std::env::var("SPELAION_TEST_DB_TOOLS") {
        return spelaion_core::backup::ToolSource::Folder(PathBuf::from(folder));
    }
    let mut leading: Vec<String> = ["run", "--rm", "-i", "-e", "PGPASSWORD"]
        .map(String::from)
        .to_vec();
    let reach_host = if cfg!(target_os = "linux") {
        leading.extend(["--network".into(), "host".into()]);
        None
    } else {
        Some("host.docker.internal".to_string())
    };
    leading.push("postgis/postgis:17-3.5-alpine".into());
    spelaion_core::backup::ToolSource::Launcher {
        program: PathBuf::from("docker"),
        leading_args: leading,
        reach_host,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_dump_task_runs_the_vendors_tool_and_never_overwrites_a_dump() {
    let Some(address) = test_pg() else { return };
    let fx = Fixture::new("dump", &address);
    let id = fx.save(
        "Nightly dump",
        "~/numbers",
        TaskRequest::Dump(spelaion_core::task::TaskDumpRequest {
            tools: spelaion_core::task::TaskTools::Detect,
            folder: fx.out(),
            name_pattern: "{database}-nightly".into(),
            extra_args: String::new(),
            options: spelaion_core::backup::DumpOptions::Postgres(
                spelaion_core::backup::PgDumpOptions {
                    format: spelaion_core::backup::PgFormat::Plain,
                    content: spelaion_core::backup::DumpContent::SchemaOnly,
                    ..Default::default()
                },
            ),
        }),
    );
    let mut env = fx.app(None);
    env.tools = Some(dump_tools());
    match run_from_app(id, &env, false).await {
        TaskOutcome::Ran {
            state: TaskRunState::Succeeded,
            summary,
            ..
        } => assert!(summary.contains("nightly"), "{summary}"),
        other => panic!("{other:?}"),
    }
    let dumped = std::fs::read_dir(fx.out())
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.to_string_lossy().contains("nightly"))
        .expect("the dump's file");
    let text = std::fs::read_to_string(&dumped).unwrap();
    assert!(text.contains("PostgreSQL database dump"), "{text}");
    // The same name again is refused, never written over: the log says so.
    match run_from_app(id, &env, false).await {
        TaskOutcome::Ran {
            state: TaskRunState::Failed,
            message,
            ..
        } => assert!(message.unwrap().contains("already exists")),
        other => panic!("{other:?}"),
    }
    assert_eq!(std::fs::read_to_string(&dumped).unwrap(), text);
}
