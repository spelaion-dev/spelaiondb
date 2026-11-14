//! The signed catalogue and the user's plugin store, end to end (ADR-055): a build of ours is
//! installed and started only as the catalogue we signed names it. Each test tries one way
//! around it.
//!
//! The test plugin stands for a plugin of ours. The tests sign their own catalogues, as
//! minisign does, with a key of their own: the key compiled into the app is never theirs.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use blake2::{Blake2b512, Digest};
use ed25519_dalek::{Signer as _, SigningKey};
use futures_util::StreamExt;
use spelaion_core::Engines;
use spelaion_core::plugins::catalogue::{Catalogue, CatalogueError, CatalogueKey};
use spelaion_core::plugins::choice::{PluginState, Source, states};
use spelaion_core::plugins::installed::{
    AppBuild, InstallError, NotUsable, PluginStore, signed_catalogue,
};
use spelaion_core::plugins::pins::{PLATFORMS, Pins, platform, program_file_name, sha256_of};
use spelaion_driver_api::{ConnectParams, DriverError, ExecEvent, ExecOptions};

const PROGRAM: &str = env!("CARGO_BIN_EXE_spelaion-test-plugin");
const NAME: &str = "spelaiondb-plugin-test";

fn here() -> &'static str {
    platform().expect("the tests run on a platform plugins are built for")
}

fn elsewhere() -> &'static str {
    PLATFORMS.into_iter().find(|p| *p != here()).unwrap()
}

/// A key pair that signs as minisign does.
struct Signer {
    key: SigningKey,
    id: [u8; 8],
}

impl Signer {
    fn new(seed: u8) -> Self {
        Self {
            key: SigningKey::from_bytes(&[seed; 32]),
            id: [seed; 8],
        }
    }

    /// Its public half, as the app would have it compiled in.
    fn public(&self) -> CatalogueKey {
        let mut bytes = b"Ed".to_vec();
        bytes.extend_from_slice(&self.id);
        bytes.extend_from_slice(&self.key.verifying_key().to_bytes());
        CatalogueKey::from_base64(&base64::engine::general_purpose::STANDARD.encode(bytes))
            .expect("the test's key")
    }

    fn signature_file(&self, algorithm: &[u8; 2], signed: &[u8]) -> String {
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let signature = self.key.sign(signed).to_bytes();
        let comment = "timestamp:0\tfile:plugins-catalogue.toml";
        let mut global = signature.to_vec();
        global.extend_from_slice(comment.as_bytes());
        let global = self.key.sign(&global).to_bytes();
        let mut first = algorithm.to_vec();
        first.extend_from_slice(&self.id);
        first.extend_from_slice(&signature);
        format!(
            "untrusted comment: signature from the test's key\n{}\ntrusted comment: {comment}\n{}\n",
            encode(&first),
            encode(&global)
        )
    }

    /// The signature file minisign writes for `text`: over its BLAKE2b-512.
    fn sign(&self, text: &[u8]) -> String {
        self.signature_file(b"ED", Blake2b512::digest(text).as_slice())
    }

    /// The form minisign used to write: over the bytes themselves.
    fn sign_legacy(&self, text: &[u8]) -> String {
        self.signature_file(b"Ed", text)
    }
}

/// A machine of the test: the folder the app is in, the user's store, and somewhere files
/// arrive.
struct Machine {
    root: PathBuf,
    signer: Signer,
    store: PluginStore,
}

impl Machine {
    fn new(label: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("spelaion-catalogue-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for folder in ["app", "downloads"] {
            std::fs::create_dir_all(root.join(folder)).unwrap();
        }
        let store = PluginStore::in_user_dir(&root.join("user"));
        Self {
            root,
            signer: Signer::new(7),
            store,
        }
    }

    fn key(&self) -> CatalogueKey {
        self.signer.public()
    }

    fn app_folder(&self) -> PathBuf {
        self.root.join("app")
    }

    /// The app this machine runs: 0.10.0, on this platform, speaking the core's protocol.
    fn app(&self) -> AppBuild {
        AppBuild {
            version: (0, 10, 0),
            ..AppBuild::this()
        }
    }

    /// A release's archive of the test plugin for `version`: the program and a licence.
    fn archive(&self, version: &str) -> PathBuf {
        let program = std::fs::read(PROGRAM).unwrap();
        self.archive_of(
            &format!("{NAME}-{version}-{}.tar.gz", here()),
            &[
                Member::file(&program_file_name(NAME), &program),
                Member::file("LICENSE", format!("version {version}").as_bytes()),
            ],
        )
    }

    fn archive_of(&self, name: &str, members: &[Member]) -> PathBuf {
        let path = self.root.join("downloads").join(name);
        let packed = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(packed);
        for member in members {
            // The name is written as it is: the tests need names a well-behaved writer refuses.
            let mut header = tar::Header::new_gnu();
            let name = member.name.as_bytes();
            header.as_old_mut().name[..name.len()].copy_from_slice(name);
            header.set_mode(0o755);
            header.set_entry_type(member.kind);
            if member.kind == tar::EntryType::Symlink {
                header.set_link_name("/etc/passwd").unwrap();
            }
            header.set_size(member.bytes.len() as u64);
            header.set_cksum();
            tar.append(&header, member.bytes.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
        path
    }

    /// Signs and accepts a catalogue.
    fn accept(&self, text: &str) -> Result<Catalogue, CatalogueError> {
        self.store.accept_catalogue_with(
            &self.key(),
            text.as_bytes(),
            &self.signer.sign(text.as_bytes()),
        )
    }

    fn catalogue(&self) -> Option<Catalogue> {
        self.store.catalogue_with(&self.key())
    }

    fn states(&self, pins: &Pins) -> Vec<PluginState> {
        states(
            pins,
            Some(&self.app_folder()),
            Some(&self.store),
            self.catalogue().as_ref(),
            &self.app(),
        )
    }

    fn state(&self, pins: &Pins) -> PluginState {
        self.states(pins)
            .into_iter()
            .find(|state| state.name == NAME)
            .expect("the test plugin is known")
    }

    fn engines(&self, pins: &Pins) -> Engines {
        Engines::builtin().with_plugin_states(&self.states(pins))
    }

    /// Puts the test plugin next to the app, as the installer would, and pins it as `version`.
    fn ship(&self, version: &str) -> (PathBuf, Pins) {
        let file = self.app_folder().join(program_file_name(NAME));
        std::fs::copy(PROGRAM, &file).unwrap();
        let text = format!(
            "schema_version = 1\n{}",
            listed(version, here(), &self.archive(version))
        );
        (file, Pins::parse(&text).expect("the test's pins"))
    }

    /// Nothing half done is left where the builds are.
    fn assert_store_is_empty(&self) {
        assert_eq!(self.store.installed(), [], "a build was left in the store");
        let ours = self.store.folder().join("ours");
        let left: Vec<_> = std::fs::read_dir(&ours)
            .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "left in the store: {left:?}");
    }
}

impl Drop for Machine {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A member of an archive, as the test writes it.
struct Member {
    name: String,
    kind: tar::EntryType,
    bytes: Vec<u8>,
}

impl Member {
    fn file(name: &str, bytes: &[u8]) -> Self {
        Self {
            name: name.to_string(),
            kind: tar::EntryType::Regular,
            bytes: bytes.to_vec(),
        }
    }

    fn link(name: &str) -> Self {
        Self {
            name: name.to_string(),
            kind: tar::EntryType::Symlink,
            bytes: Vec::new(),
        }
    }
}

/// A catalogue's `[[plugin]]` for `version`, whose build for `target` is `archive`, holding
/// the test plugin's program.
fn listed(version: &str, target: &str, archive: &Path) -> String {
    format!(
        r#"
[[plugin]]
name = "{NAME}"
repository = "spelaion-dev/{NAME}"
version = "{version}"
protocol = "1.0"
engines = ["test", "test2"]
min_app = "0.10.0"

[plugin.platform.{target}]
archive = "{}"
archive_sha256 = "{}"
archive_size = {}
program_sha256 = "{}"
"#,
        archive.file_name().unwrap().to_string_lossy(),
        sha256_of(archive).unwrap(),
        std::fs::metadata(archive).unwrap().len(),
        sha256_of(Path::new(PROGRAM)).unwrap(),
    )
}

fn catalogue(serial: u64, entries: &[String]) -> String {
    format!(
        "schema_version = 1\nserial = {serial}\n{}",
        entries.concat()
    )
}

fn withdrawn(version: &str, reason: &str) -> String {
    format!("\n[[withdrawn]]\nname = \"{NAME}\"\nversion = \"{version}\"\nreason = \"{reason}\"\n")
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
    let driver = engines.driver("test").expect("the engine has a driver");
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

async fn running(program: &Path) -> bool {
    spelaion_core::plugins::is_running(program).await
}

#[tokio::test]
async fn a_build_installed_from_the_signed_catalogue_serves_its_engines() {
    let machine = Machine::new("serves");
    let archive = machine.archive("0.2.0");
    let accepted = machine
        .accept(&catalogue(3, &[listed("0.2.0", here(), &archive)]))
        .expect("a catalogue we signed");
    assert_eq!(accepted.serial(), 3);

    // The file the user picked, or a download handed over: installed as the entry it is.
    let installed = machine
        .store
        .install(&accepted, &machine.app(), &archive)
        .expect("the catalogue's archive");
    assert_eq!(installed.name, NAME);
    assert_eq!(installed.version, "0.2.0");
    assert!(installed.program.starts_with(machine.store.folder()));
    assert_eq!(machine.store.installed(), std::slice::from_ref(&installed));

    let state = machine.state(&Pins::none());
    let chosen = state
        .chosen
        .as_ref()
        .expect("the installed build is chosen");
    assert_eq!(chosen.source, Source::Installed);
    assert_eq!(chosen.program, installed.program);
    assert_eq!(state.update(), None, "it is the newest there is");

    let engines = machine.engines(&Pins::none());
    let pid = pid_through(&engines).await.unwrap();
    assert_ne!(pid, std::process::id().to_string());
    assert!(running(&installed.program).await);
    within(engines.driver("test2").unwrap().connect(&params()))
        .await
        .unwrap();

    // Installed again, the same build is left as it is (it is running).
    let again = machine
        .store
        .install_as(&accepted, &machine.app(), NAME, (0, 2, 0), &archive)
        .unwrap();
    assert_eq!(again, installed);
    assert!(running(&installed.program).await);
}

/// Only our key's signature of exactly those bytes makes a catalogue.
#[tokio::test]
async fn a_catalogue_we_did_not_sign_is_not_accepted() {
    let machine = Machine::new("unsigned");
    let archive = machine.archive("0.2.0");
    let text = catalogue(3, &[listed("0.2.0", here(), &archive)]);
    let ours = machine.signer.sign(text.as_bytes());
    let other_text = catalogue(4, &[listed("0.2.0", here(), &archive)]);
    let someone = Signer::new(9);
    // Someone else's key under our key's id: the id is only a label.
    let impostor = Signer {
        key: SigningKey::from_bytes(&[9; 32]),
        id: [7; 8],
    };

    let attempts: Vec<(&str, String, String)> = vec![
        (
            "signed by another key",
            text.clone(),
            someone.sign(text.as_bytes()),
        ),
        (
            "signed by another key that claims our key's id",
            text.clone(),
            impostor.sign(text.as_bytes()),
        ),
        (
            "changed after it was signed",
            other_text.clone(),
            ours.clone(),
        ),
        (
            "one byte added after it was signed",
            format!("{text}\n"),
            ours.clone(),
        ),
        (
            "signed the old way, over the bytes themselves",
            text.clone(),
            machine.signer.sign_legacy(text.as_bytes()),
        ),
        ("with no signature", text.clone(), String::new()),
        (
            "with something that is not a signature",
            text.clone(),
            "untrusted comment: x\nAAAA\ntrusted comment: y\nBBBB\n".into(),
        ),
        (
            "with the trusted comment changed",
            text.clone(),
            ours.replace("timestamp:0", "timestamp:1"),
        ),
    ];
    for (what, text, signature) in attempts {
        let refused =
            machine
                .store
                .accept_catalogue_with(&machine.key(), text.as_bytes(), &signature);
        assert!(
            matches!(refused, Err(CatalogueError::Signature(_))),
            "a catalogue {what} was not refused for its signature: {refused:?}"
        );
        assert!(machine.catalogue().is_none(), "{what}: something was kept");
    }
    // Signed by us, and not a catalogue: read whole or not at all.
    let odd = format!("{text}\nurl = \"https://evil.example\"\n");
    assert!(matches!(
        machine.accept(&odd),
        Err(CatalogueError::Syntax(_) | CatalogueError::Entry { .. })
    ));
    let huge = format!("{text}\n# {}\n", "x".repeat(1 << 20));
    assert!(matches!(
        machine.accept(&huge),
        Err(CatalogueError::TooLarge(_))
    ));
    assert!(machine.catalogue().is_none());

    // With nothing accepted, the archive is no plugin, and nothing in the store is started.
    assert!(machine.states(&Pins::none()).is_empty());
    // The one we did sign is accepted.
    machine
        .store
        .accept_catalogue_with(&machine.key(), text.as_bytes(), &ours)
        .expect("ours");
}

/// The catalogue is published, and can be given by the user, as one file: its signature and
/// then its text. It is the same catalogue, under the same checks.
#[test]
fn a_catalogue_given_as_one_signed_file_is_checked_the_same() {
    let machine = Machine::new("one-file");
    let archive = machine.archive("0.2.0");
    let text = catalogue(3, &[listed("0.2.0", here(), &archive)]);
    let signed = signed_catalogue(&machine.signer.sign(text.as_bytes()), text.as_bytes());

    // Its text swapped for another, under the first one's signature.
    let other = catalogue(9, &[listed("0.2.0", here(), &archive)]);
    let swapped = signed_catalogue(&machine.signer.sign(text.as_bytes()), other.as_bytes());
    for (what, file) in [
        ("with another text", swapped.as_slice()),
        ("with no signature", text.as_bytes()),
        ("that is empty", b"".as_slice()),
        ("cut short", &signed[..signed.len() / 2]),
    ] {
        let refused = machine
            .store
            .accept_signed_catalogue_with(&machine.key(), file);
        assert!(
            matches!(refused, Err(CatalogueError::Signature(_))),
            "a file {what}: {refused:?}"
        );
    }
    assert!(machine.catalogue().is_none());

    let accepted = machine
        .store
        .accept_signed_catalogue_with(&machine.key(), &signed)
        .expect("the file we signed");
    assert_eq!(accepted.serial(), 3);
    // What is kept is that same file: it can be handed to another machine as it is.
    let kept = std::fs::read(machine.store.folder().join("catalogue.signed")).unwrap();
    assert_eq!(kept, signed);
    // The app's own way in still takes only its own key.
    assert!(machine.store.accept_signed_catalogue(&signed).is_err());
}

/// The key is the one compiled into the app: the app's own way in takes no key from anywhere.
#[test]
fn the_app_accepts_only_what_its_own_key_signed() {
    let machine = Machine::new("app-key");
    let archive = machine.archive("0.2.0");
    let text = catalogue(3, &[listed("0.2.0", here(), &archive)]);
    // Validly signed, by a key that is not the app's.
    let refused = machine
        .store
        .accept_catalogue(text.as_bytes(), &machine.signer.sign(text.as_bytes()));
    match CatalogueKey::shipped() {
        None => assert!(matches!(refused, Err(CatalogueError::NoKey)), "{refused:?}"),
        Some(_) => assert!(
            matches!(refused, Err(CatalogueError::Signature(_))),
            "{refused:?}"
        ),
    }
    assert!(machine.store.catalogue().is_none());
    // And one the test's key did get accepted is still nothing to the app.
    machine.accept(&text).unwrap();
    assert!(machine.store.catalogue().is_none());
}

#[tokio::test]
async fn an_older_catalogue_never_replaces_a_newer_one() {
    let machine = Machine::new("serial");
    let archive = machine.archive("0.2.0");
    let entry = listed("0.2.0", here(), &archive);
    let five = catalogue(5, std::slice::from_ref(&entry));
    machine.accept(&five).unwrap();

    // An older one we did sign, which no longer withdraws anything or lists an old build.
    let refused = machine.accept(&catalogue(4, std::slice::from_ref(&entry)));
    assert!(
        matches!(
            refused,
            Err(CatalogueError::Older {
                offered: 4,
                accepted: 5
            })
        ),
        "{refused:?}"
    );
    // Another catalogue under the serial already accepted.
    let other = catalogue(5, &[entry.clone(), withdrawn("0.1.0", "Broken.")]);
    assert!(matches!(
        machine.accept(&other),
        Err(CatalogueError::SameSerial(5))
    ));
    assert_eq!(machine.catalogue().unwrap().serial(), 5);
    assert!(machine.catalogue().unwrap().withdrawn().is_empty());

    // The same one again is fine, and a newer one replaces it.
    machine.accept(&five).unwrap();
    machine
        .accept(&catalogue(6, &[entry, withdrawn("0.1.0", "Broken.")]))
        .unwrap();
    let kept = machine.catalogue().unwrap();
    assert_eq!(kept.serial(), 6);
    assert_eq!(kept.why_withdrawn(NAME, (0, 1, 0)), Some("Broken."));
}

/// The store is not trusted: the catalogue kept there is checked again every time, so editing
/// it gives nothing.
#[tokio::test]
async fn a_catalogue_edited_in_the_store_is_no_catalogue() {
    let machine = Machine::new("edited");
    let archive = machine.archive("0.2.0");
    let accepted = machine
        .accept(&catalogue(3, &[listed("0.2.0", here(), &archive)]))
        .unwrap();
    let installed = machine
        .store
        .install(&accepted, &machine.app(), &archive)
        .unwrap();
    assert!(machine.state(&Pins::none()).chosen.is_some());

    // Someone edits the kept file so that it names another program's hash.
    let file = machine.store.folder().join("catalogue.signed");
    let kept = std::fs::read_to_string(&file).unwrap();
    let program = sha256_of(Path::new(PROGRAM)).unwrap();
    assert!(kept.contains(&program));
    std::fs::write(&file, kept.replace(&program, &"0".repeat(64))).unwrap();

    assert!(
        machine.catalogue().is_none(),
        "an edited file was read as a catalogue"
    );
    let state = machine.state(&Pins::none());
    assert_eq!(state.chosen, None, "a build was chosen with no catalogue");
    assert_eq!(state.refused[0].why, NotUsable::Unlisted);
    assert!(machine.engines(&Pins::none()).driver("test").is_none());
    assert!(!running(&installed.program).await);
}

/// Being in the store does not make a file a plugin: only what the catalogue names is started,
/// and only while it is that build.
#[tokio::test]
async fn a_file_dropped_in_the_store_is_not_a_plugin() {
    let machine = Machine::new("dropped");
    let archive = machine.archive("0.2.0");
    machine
        .accept(&catalogue(3, &[listed("0.2.0", here(), &archive)]))
        .unwrap();

    // The program, put by hand where a version the catalogue does not list would be.
    let dropped = machine.store.program(NAME, (9, 9, 9));
    std::fs::create_dir_all(dropped.parent().unwrap()).unwrap();
    std::fs::copy(PROGRAM, &dropped).unwrap();
    // And under names no plugin of ours has.
    for odd in ["spelaiondb-plugin-evil", "plugin", "..hidden"] {
        let file = machine
            .store
            .folder()
            .join("ours")
            .join(odd)
            .join("1.0.0")
            .join(program_file_name(odd));
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::copy(PROGRAM, &file).unwrap();
    }

    let states = machine.states(&Pins::none());
    assert!(
        states.iter().all(|state| state.chosen.is_none()),
        "a dropped file was chosen: {states:?}"
    );
    let state = machine.state(&Pins::none());
    assert_eq!(state.refused[0].version, "9.9.9");
    assert_eq!(state.refused[0].why, NotUsable::Unlisted);
    // Its engines say so; they do not fall back to anything, and nothing is started.
    let engines = machine.engines(&Pins::none());
    let error = pid_through(&engines).await.unwrap_err().to_string();
    assert!(error.contains("accepted lists it"), "{error}");
    assert!(error.contains("Version 0.2.0 can be installed"), "{error}");
    assert!(!running(&dropped).await);

    // A file put where a listed version lives is started only if it is that build.
    let listed_place = machine.store.program(NAME, (0, 2, 0));
    std::fs::create_dir_all(listed_place.parent().unwrap()).unwrap();
    let mut bytes = std::fs::read(PROGRAM).unwrap();
    bytes.push(0);
    std::fs::write(&listed_place, bytes).unwrap();
    let engines = machine.engines(&Pins::none());
    let error = pid_through(&engines).await.unwrap_err().to_string();
    assert!(
        error.contains("is not the build that was pinned"),
        "{error}"
    );
    assert!(!running(&listed_place).await);
}

#[tokio::test]
async fn an_archive_is_installed_only_as_its_catalogue_entry() {
    let machine = Machine::new("entry");
    let app = machine.app();
    let archive = machine.archive("0.2.0");
    let entry = listed("0.2.0", here(), &archive);
    let accepted = machine
        .accept(&catalogue(3, std::slice::from_ref(&entry)))
        .unwrap();

    // A file the catalogue does not list: someone else's plugin, or anything at all.
    let unknown = machine.archive("0.2.1");
    let refused = machine.store.install(&accepted, &app, &unknown);
    assert!(
        matches!(refused, Err(InstallError::NotInCatalogue { .. })),
        "{refused:?}"
    );
    // Given as the listed version, it is still not that version's archive.
    let refused = machine
        .store
        .install_as(&accepted, &app, NAME, (0, 2, 0), &unknown);
    assert!(
        matches!(refused, Err(InstallError::NotTheBuild { .. })),
        "{refused:?}"
    );
    // A version the catalogue does not have.
    let refused = machine
        .store
        .install_as(&accepted, &app, NAME, (0, 3, 0), &archive);
    assert!(
        matches!(
            refused,
            Err(InstallError::NotUsable {
                why: NotUsable::Unlisted,
                ..
            })
        ),
        "{refused:?}"
    );
    machine.assert_store_is_empty();

    // The catalogue's size is checked too.
    let size = std::fs::metadata(&archive).unwrap().len();
    let wrong_size = entry.replace(
        &format!("archive_size = {size}"),
        &format!("archive_size = {}", size + 1),
    );
    let accepted = machine.accept(&catalogue(4, &[wrong_size])).unwrap();
    let refused = machine.store.install(&accepted, &app, &archive);
    assert!(
        matches!(refused, Err(InstallError::Size { .. })),
        "{refused:?}"
    );

    // The right archive, whose program is not the one the entry pins.
    let program = sha256_of(Path::new(PROGRAM)).unwrap();
    let other_program = entry.replace(
        &format!("program_sha256 = \"{program}\""),
        &format!("program_sha256 = \"{}\"", "c".repeat(64)),
    );
    let accepted = machine.accept(&catalogue(5, &[other_program])).unwrap();
    let refused = machine.store.install(&accepted, &app, &archive);
    assert!(
        matches!(&refused, Err(InstallError::Program { expected, found, .. })
            if expected == &"c".repeat(64) && found == &program),
        "{refused:?}"
    );
    machine.assert_store_is_empty();
}

/// An archive is not trusted for being listed: only ordinary files at its top are read, and
/// only the program is written, inside the store.
#[tokio::test]
async fn an_archive_that_is_not_a_release_is_not_unpacked() {
    let machine = Machine::new("archive");
    let app = machine.app();
    let program = std::fs::read(PROGRAM).unwrap();
    let file = program_file_name(NAME);
    let name = |n: u32| format!("{NAME}-0.2.{n}-{}.tar.gz", here());

    let odd: Vec<(&str, PathBuf)> = vec![
        (
            "a member that climbs out",
            machine.archive_of(
                &name(0),
                &[
                    Member::file(&file, &program),
                    Member::file("../escaped", b"out"),
                ],
            ),
        ),
        (
            "the program in a folder",
            machine.archive_of(&name(1), &[Member::file(&format!("bin/{file}"), &program)]),
        ),
        (
            "an absolute path",
            machine.archive_of(
                &name(2),
                &[
                    Member::file(&file, &program),
                    Member::file("/tmp/spelaion-escaped", b"out"),
                ],
            ),
        ),
        (
            "the program as a link",
            machine.archive_of(&name(3), &[Member::link(&file)]),
        ),
        (
            "a link next to the program",
            machine.archive_of(
                &name(4),
                &[Member::file(&file, &program), Member::link("LICENSE")],
            ),
        ),
        (
            "the program twice",
            machine.archive_of(
                &name(5),
                &[Member::file(&file, &program), Member::file(&file, b"other")],
            ),
        ),
        (
            "no program",
            machine.archive_of(&name(6), &[Member::file("LICENSE", b"Apache-2.0")]),
        ),
        ("not an archive at all", {
            let path = machine.root.join("downloads").join(name(7));
            std::fs::write(&path, b"MZ this is a program, not an archive").unwrap();
            path
        }),
        (
            "a zip by its name",
            machine.archive_of(
                &format!("{NAME}-0.2.8-{}.zip", here()),
                &[Member::file(&file, &program)],
            ),
        ),
    ];
    let entries: Vec<String> = odd
        .iter()
        .enumerate()
        .map(|(n, (_, archive))| listed(&format!("0.2.{n}"), here(), archive))
        .collect();
    let accepted = machine.accept(&catalogue(3, &entries)).unwrap();

    for (what, archive) in &odd {
        let refused = machine.store.install(&accepted, &app, archive);
        assert!(
            matches!(refused, Err(InstallError::Archive { .. })),
            "an archive with {what} was not refused as an archive: {refused:?}"
        );
        machine.assert_store_is_empty();
    }
    // Nothing was written outside the store.
    assert!(
        !machine
            .root
            .join("user")
            .join("plugins")
            .join("escaped")
            .exists()
    );
    assert!(!machine.store.folder().join("ours").join("escaped").exists());
    assert!(!Path::new("/tmp/spelaion-escaped").exists());
}

/// A build we withdrew is not started from the next catalogue accepted, wherever it is; the
/// user is told why and what replaces it.
#[tokio::test]
async fn a_withdrawn_build_is_refused_wherever_it_is() {
    let machine = Machine::new("withdrawn");
    let app = machine.app();
    let old = machine.archive("0.2.0");
    let entries = [listed("0.2.0", here(), &old)];
    let accepted = machine.accept(&catalogue(3, &entries)).unwrap();
    let installed = machine.store.install(&accepted, &app, &old).unwrap();
    // And the build inside the installer.
    let (shipped, pins) = machine.ship("0.1.0");
    let engines = machine.engines(&pins);
    pid_through(&engines).await.unwrap();
    assert!(
        running(&installed.program).await,
        "the newest is the one started"
    );
    spelaion_core::plugins::stop(&installed.program).await;

    // Both are withdrawn; 0.3.0 replaces them.
    let new = machine.archive("0.3.0");
    let next = catalogue(
        4,
        &[
            listed("0.2.0", here(), &old),
            listed("0.3.0", here(), &new),
            withdrawn("0.2.0", "It lost rows on import."),
            withdrawn("0.1.0", "It could not cancel."),
        ],
    );
    let accepted = machine.accept(&next).unwrap();

    let state = machine.state(&pins);
    assert_eq!(state.chosen, None);
    let reasons: Vec<_> = state
        .refused
        .iter()
        .map(|refused| {
            (
                refused.version.as_str(),
                refused.source,
                refused.why.clone(),
            )
        })
        .collect();
    assert_eq!(
        reasons,
        [
            (
                "0.2.0",
                Source::Installed,
                NotUsable::Withdrawn("It lost rows on import.".into())
            ),
            (
                "0.1.0",
                Source::Shipped,
                NotUsable::Withdrawn("It could not cancel.".into())
            ),
        ]
    );
    assert_eq!(state.offered.as_ref().unwrap().version, "0.3.0");

    let engines = machine.engines(&pins);
    let error = pid_through(&engines).await.unwrap_err().to_string();
    assert!(error.contains("It lost rows on import."), "{error}");
    assert!(error.contains("Version 0.3.0 can be installed"), "{error}");
    assert!(!running(&installed.program).await);
    assert!(!running(&shipped).await);

    // A withdrawn build cannot be installed again either.
    let refused = machine.store.install(&accepted, &app, &old);
    assert!(
        matches!(
            refused,
            Err(InstallError::NotUsable {
                why: NotUsable::Withdrawn(_),
                ..
            })
        ),
        "{refused:?}"
    );

    // Its replacement is installed on the user's click, and is the one started.
    let replacement = machine.store.install(&accepted, &app, &new).unwrap();
    let engines = machine.engines(&pins);
    pid_through(&engines).await.unwrap();
    assert!(running(&replacement.program).await);
    assert!(!running(&installed.program).await);

    // Withdrawn and no longer listed at all: still refused, by name and version.
    let later = catalogue(
        5,
        &[
            listed("0.3.0", here(), &new),
            withdrawn("0.2.0", "It lost rows on import."),
        ],
    );
    machine.accept(&later).unwrap();
    let state = machine.state(&Pins::none());
    assert_eq!(state.chosen.as_ref().unwrap().version, "0.3.0");
    assert_eq!(
        state.refused[0].why,
        NotUsable::Withdrawn("It lost rows on import.".into())
    );
}

/// What protects an app from a build made for a newer one: the entry's protocol version and
/// its lowest app version.
#[tokio::test]
async fn a_build_this_app_cannot_use_is_neither_installed_nor_started() {
    let machine = Machine::new("too-new");
    let app = machine.app();
    let cases: Vec<(&str, &str, &str, &str)> = vec![
        (
            "0.4.0",
            "protocol = \"1.0\"",
            "protocol = \"2.0\"",
            "plugin protocol",
        ),
        (
            "0.5.0",
            "protocol = \"1.0\"",
            "protocol = \"1.99\"",
            "plugin protocol",
        ),
        (
            "0.6.0",
            "protocol = \"1.0\"",
            "protocol = \"0.9\"",
            "plugin protocol",
        ),
        (
            "0.7.0",
            "min_app = \"0.10.0\"",
            "min_app = \"0.10.1\"",
            "needs SpelaionDB 0.10.1",
        ),
    ];
    let good = machine.archive("0.2.0");
    let mut entries = vec![listed("0.2.0", here(), &good)];
    let mut archives = Vec::new();
    for (version, from, to, _) in &cases {
        let archive = machine.archive(version);
        entries.push(listed(version, here(), &archive).replace(from, to));
        archives.push(archive);
    }
    let accepted = machine.accept(&catalogue(3, &entries)).unwrap();

    for ((version, _, _, says), archive) in cases.iter().zip(&archives) {
        let refused = machine.store.install(&accepted, &app, archive);
        let Err(InstallError::NotUsable { why, .. }) = &refused else {
            panic!("{version} was not refused: {refused:?}");
        };
        assert!(why.to_string().contains(says), "{version}: {why}");
        // Put there by hand, as the build it is: still not started.
        let program = machine.store.program(NAME, spelaion_version(version));
        std::fs::create_dir_all(program.parent().unwrap()).unwrap();
        std::fs::copy(PROGRAM, &program).unwrap();
    }

    let state = machine.state(&Pins::none());
    assert_eq!(state.chosen, None);
    assert_eq!(state.refused.len(), 4);
    // What is offered is the newest this app can use, not the newest there is.
    assert_eq!(state.offered.as_ref().unwrap().version, "0.2.0");
    let engines = machine.engines(&Pins::none());
    let error = pid_through(&engines).await.unwrap_err().to_string();
    assert!(error.contains("0.7.0 is not started"), "{error}");
    for (version, ..) in &cases {
        assert!(!running(&machine.store.program(NAME, spelaion_version(version))).await);
    }

    // The one it can use is installed and started, under the newer ones it cannot.
    let installed = machine.store.install(&accepted, &app, &good).unwrap();
    let state = machine.state(&Pins::none());
    assert_eq!(state.chosen.as_ref().unwrap().version, "0.2.0");
    assert_eq!(state.update(), None, "nothing newer is offered to this app");
    pid_through(&machine.engines(&Pins::none())).await.unwrap();
    assert!(running(&installed.program).await);

    // A newer app is offered more.
    let newer_app = AppBuild {
        version: (0, 10, 1),
        ..machine.app()
    };
    let states = states(
        &Pins::none(),
        None,
        Some(&machine.store),
        machine.catalogue().as_ref(),
        &newer_app,
    );
    assert_eq!(states[0].chosen.as_ref().unwrap().version, "0.7.0");
}

fn spelaion_version(text: &str) -> (u32, u32, u32) {
    let mut parts = text.split('.').map(|part| part.parse().unwrap());
    (
        parts.next().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap(),
    )
}

#[tokio::test]
async fn the_newest_build_on_this_machine_is_the_one_started() {
    let machine = Machine::new("newest");
    let app = machine.app();
    let (shipped, pins) = machine.ship("0.1.0");
    // With no catalogue, the build inside the installer works by itself.
    let state = machine.state(&pins);
    assert_eq!(state.chosen.as_ref().unwrap().source, Source::Shipped);
    assert_eq!(state.offered, None);
    pid_through(&machine.engines(&pins)).await.unwrap();
    assert!(running(&shipped).await);
    spelaion_core::plugins::stop(&shipped).await;

    // The catalogue lists newer ones: an update is offered, and nothing is installed by itself.
    let two = machine.archive("0.2.0");
    let three = machine.archive("0.3.0");
    let accepted = machine
        .accept(&catalogue(
            3,
            &[
                listed("0.1.0", here(), &machine.archive("0.1.0")),
                listed("0.2.0", here(), &two),
                listed("0.3.0", here(), &three),
            ],
        ))
        .unwrap();
    let state = machine.state(&pins);
    assert_eq!(state.chosen.as_ref().unwrap().version, "0.1.0");
    let update = state.update().expect("an update is offered");
    assert_eq!(update.version, "0.3.0");
    assert_eq!(
        update.archive_size,
        std::fs::metadata(&three).unwrap().len()
    );
    assert_eq!(
        update.address,
        format!(
            "https://github.com/spelaion-dev/{NAME}/releases/download/v0.3.0/{}",
            three.file_name().unwrap().to_string_lossy()
        )
    );
    assert_eq!(machine.store.installed(), []);

    // 0.2.0 is installed: newer than the one inside the installer, so it is the one started.
    let installed = machine.store.install(&accepted, &app, &two).unwrap();
    let state = machine.state(&pins);
    assert_eq!(state.chosen.as_ref().unwrap().version, "0.2.0");
    assert_eq!(state.chosen.as_ref().unwrap().source, Source::Installed);
    assert_eq!(state.update().unwrap().version, "0.3.0");
    pid_through(&machine.engines(&pins)).await.unwrap();
    assert!(running(&installed.program).await);
    assert!(!running(&shipped).await);

    // The same version in both places: the one inside the installer.
    let same = machine
        .store
        .install(&accepted, &app, &machine.archive("0.1.0"))
        .unwrap();
    assert_eq!(same.version, "0.1.0");
    machine.store.remove(NAME, (0, 2, 0)).await.unwrap();
    let state = machine.state(&pins);
    assert_eq!(state.chosen.as_ref().unwrap().version, "0.1.0");
    assert_eq!(state.chosen.as_ref().unwrap().source, Source::Shipped);
}

#[tokio::test]
async fn a_build_for_another_platform_is_neither_offered_nor_installed() {
    let machine = Machine::new("platform");
    let archive = machine.archive("0.2.0");
    let accepted = machine
        .accept(&catalogue(3, &[listed("0.2.0", elsewhere(), &archive)]))
        .unwrap();
    let state = machine.state(&Pins::none());
    assert_eq!(state.offered, None);
    assert_eq!(state.chosen, None);

    let refused = machine.store.install(&accepted, &machine.app(), &archive);
    assert!(
        matches!(&refused, Err(InstallError::OtherPlatform { platform, .. })
            if platform == elsewhere()),
        "{refused:?}"
    );
    let refused = machine
        .store
        .install_as(&accepted, &machine.app(), NAME, (0, 2, 0), &archive);
    assert!(
        matches!(
            refused,
            Err(InstallError::NotUsable {
                why: NotUsable::NoBuild,
                ..
            })
        ),
        "{refused:?}"
    );
    machine.assert_store_is_empty();

    // Put there by hand, it is still a build for no platform of this machine.
    let program = machine.store.program(NAME, (0, 2, 0));
    std::fs::create_dir_all(program.parent().unwrap()).unwrap();
    std::fs::copy(PROGRAM, &program).unwrap();
    let state = machine.state(&Pins::none());
    assert_eq!(state.chosen, None);
    assert_eq!(state.refused[0].why, NotUsable::NoBuild);
    // On a computer no plugin is built for, nothing is installed at all.
    let nowhere = AppBuild {
        platform: None,
        ..machine.app()
    };
    assert!(matches!(
        machine.store.install(&accepted, &nowhere, &archive),
        Err(InstallError::NoPlatform)
    ));
}

#[tokio::test]
async fn removing_a_build_stops_it_and_takes_its_engines_away() {
    let machine = Machine::new("remove");
    let archive = machine.archive("0.2.0");
    let accepted = machine
        .accept(&catalogue(3, &[listed("0.2.0", here(), &archive)]))
        .unwrap();
    let installed = machine
        .store
        .install(&accepted, &machine.app(), &archive)
        .unwrap();
    pid_through(&machine.engines(&Pins::none())).await.unwrap();
    assert!(running(&installed.program).await);

    within(machine.store.remove(NAME, (0, 2, 0))).await.unwrap();
    assert!(!running(&installed.program).await);
    assert!(!installed.program.exists());
    machine.assert_store_is_empty();
    assert!(machine.engines(&Pins::none()).driver("test").is_none());
    // Removing what is not there is not an error; a name that is not a plugin's is.
    machine.store.remove(NAME, (0, 2, 0)).await.unwrap();
    assert!(machine.store.remove("../..", (0, 2, 0)).await.is_err());
}
