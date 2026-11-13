//! The pins, end to end (ADR-042): only a pinned program is started, and only while its file is
//! the pinned build. Each test tries one way around it.
//!
//! The test plugin stands for a plugin of ours: its program is copied next to "the app" (a
//! folder of the test) under a pinned name, and pinned with its real hash.

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures_util::StreamExt;
use spelaion_core::Engines;
use spelaion_core::plugins::pins::{Pins, platform, program_file_name, sha256_of};
use spelaion_driver_api::{ConnectParams, DriverError, ExecEvent, ExecOptions};

const PROGRAM: &str = env!("CARGO_BIN_EXE_spelaion-test-plugin");
const NAME: &str = "spelaiondb-plugin-test";

/// A folder that stands for the one the app is in.
struct AppFolder(PathBuf);

impl AppFolder {
    fn new(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("spelaion-pins-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// Puts the test plugin there as the program called `name`.
    fn install(&self, name: &str) -> PathBuf {
        let file = self.0.join(program_file_name(name));
        std::fs::copy(PROGRAM, &file).unwrap();
        file
    }
}

impl Drop for AppFolder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A `plugins.toml` that pins `NAME` for `target` with the program hash `sha256`.
fn pins_for(target: &str, sha256: &str) -> Pins {
    let zeros = "0".repeat(64);
    Pins::parse(&format!(
        r#"schema_version = 1

[[plugin]]
name = "{NAME}"
repository = "spelaion-dev/{NAME}"
version = "0.1.0"
protocol = "1.0"
engines = ["test", "test2"]
min_app = "0.10.0"

[plugin.platform.{target}]
archive = "{NAME}-0.1.0-{target}.zip"
archive_sha256 = "{zeros}"
archive_size = 1
program_sha256 = "{sha256}"
"#
    ))
    .expect("the test's pins")
}

fn here() -> &'static str {
    platform().expect("the tests run on a platform plugins are built for")
}

fn params() -> ConnectParams {
    ConnectParams {
        host: "db.internal".into(),
        port: 5432,
        user: "app".into(),
        database: "made".into(),
        ..Default::default()
    }
}

async fn within<T>(what: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), what)
        .await
        .expect("it took too long")
}

/// The process id the engine `test` answers from, or why it could not be reached.
async fn pid_through(engines: &Engines) -> Result<String, DriverError> {
    let driver = engines.driver("test").expect("the engine is pinned");
    let connection = within(driver.connect(&params())).await?;
    let mut events = connection.execute("pid", &ExecOptions::default()).await?;
    let mut pid = String::new();
    while let Some(event) = within(events.next()).await {
        if let ExecEvent::Batch(batch) = event? {
            let column = batch
                .column(0)
                .as_any()
                .downcast_ref::<arrow::array::StringArray>()
                .expect("a text column");
            pid = column.value(0).to_string();
        }
    }
    Ok(pid)
}

fn engines_of(pins: &Pins, folder: &Path) -> Engines {
    Engines::builtin().with_pinned_plugins(pins, folder)
}

#[tokio::test]
async fn a_pinned_program_next_to_the_app_serves_its_engines() {
    let app = AppFolder::new("serves");
    let file = app.install(NAME);
    let pins = pins_for(here(), &sha256_of(&file).unwrap());
    let engines = engines_of(&pins, &app.0);

    let pid = pid_through(&engines).await.unwrap();
    assert_ne!(
        pid,
        std::process::id().to_string(),
        "it runs in its own process"
    );
    // Both of its engines, from the one process.
    let other = engines
        .driver("test2")
        .expect("the second engine is pinned");
    within(other.connect(&params())).await.unwrap();
}

/// A file with the pinned name that is not the pinned build is never started, and the engine
/// does not quietly fall back to anything: the error says what was found.
#[tokio::test]
async fn a_swapped_file_is_refused_and_never_started() {
    let app = AppFolder::new("swapped");
    let file = app.install(NAME);
    let pinned = sha256_of(&file).unwrap();
    // The same program with one byte more: it would run, and it is not what was pinned.
    let mut bytes = std::fs::read(&file).unwrap();
    bytes.push(0);
    std::fs::write(&file, bytes).unwrap();
    let engines = engines_of(&pins_for(here(), &pinned), &app.0);

    let error = pid_through(&engines).await.unwrap_err();
    let text = error.to_string();
    assert!(text.contains("is not the build that was pinned"), "{text}");
    assert!(text.contains(&pinned), "{text}");
    assert!(
        !spelaion_core::plugins::is_running(&file).await,
        "the swapped file was started"
    );
    // Asked again, refused again: the check is not remembered.
    assert!(pid_through(&engines).await.is_err());
    assert!(!spelaion_core::plugins::is_running(&file).await);
}

/// The hash is checked every time the process is about to start, not only the first.
#[tokio::test]
async fn a_file_changed_after_it_ran_is_refused_at_the_next_start() {
    let app = AppFolder::new("changed-later");
    let file = app.install(NAME);
    let pins = pins_for(here(), &sha256_of(&file).unwrap());
    let engines = engines_of(&pins, &app.0);
    pid_through(&engines).await.unwrap();
    assert!(spelaion_core::plugins::is_running(&file).await);

    // The plugin stops, and its file is replaced before it is needed again.
    spelaion_core::plugins::stop(&file).await;
    let mut bytes = std::fs::read(&file).unwrap();
    bytes.push(0);
    for _ in 0..100 {
        // Windows keeps a program's file locked until its process is gone.
        if std::fs::write(&file, &bytes).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let error = pid_through(&engines).await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("is not the build that was pinned"),
        "{error}"
    );
}

/// A pin for another platform is not a pin here: that hash never lets this platform's file in.
#[tokio::test]
async fn a_hash_pinned_for_another_platform_starts_nothing_here() {
    let app = AppFolder::new("other-platform");
    let file = app.install(NAME);
    let other = spelaion_core::plugins::pins::PLATFORMS
        .into_iter()
        .find(|p| *p != here())
        .unwrap();
    let pins = pins_for(other, &sha256_of(&file).unwrap());
    let engines = engines_of(&pins, &app.0);
    assert!(
        engines.driver("test").is_none(),
        "the engine has no plugin here"
    );
}

/// A program next to the app that no pin names is not a plugin, whatever it is called and
/// whatever it would serve.
#[tokio::test]
async fn a_program_that_is_not_pinned_is_never_started() {
    let app = AppFolder::new("unpinned");
    let pinned = app.install(NAME);
    let pins = pins_for(here(), &sha256_of(&pinned).unwrap());
    std::fs::remove_file(&pinned).unwrap();
    // The same program under other names, one of them close to the pinned one.
    for name in [
        "spelaiondb-plugin-evil",
        "spelaiondb-plugin-test2",
        "plugin",
    ] {
        app.install(name);
    }
    let engines = engines_of(&pins, &app.0);
    assert!(
        engines.driver("test").is_none(),
        "an engine was taken from a program that is not pinned"
    );
    // With no pins at all, nothing in the folder is a plugin.
    app.install(NAME);
    assert!(engines_of(&Pins::none(), &app.0).driver("test").is_none());
}

/// The pins a build carries are the ones it was built with: there is no file, variable or
/// setting that a release build reads them from.
#[test]
fn the_shipped_pins_come_from_the_build() {
    let shipped = Pins::shipped();
    // Whatever is pinned today, it is what `plugins.toml` said when this was compiled.
    let text = include_str!("../../../plugins.toml");
    let parsed = Pins::parse(text).expect("plugins.toml");
    let names =
        |pins: &Pins| -> Vec<String> { pins.plugins().iter().map(|p| p.name.clone()).collect() };
    assert_eq!(names(shipped), names(&parsed));
}
