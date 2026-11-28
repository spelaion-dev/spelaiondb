//! Other people's plugins, end to end (ADR-041, point 5; ADR-055, point 5): added only by the
//! user, from a file or an address, after a dialog that showed what arrived; pinned by hash;
//! unsupported and said so; nothing for agents and no read-only mode. Each test tries one way
//! around a rule.
//!
//! The test plugin stands for someone else's plugin: a program with a manifest of its own in an
//! archive, serving the made-up engine `test`.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spelaion_config::{ConnectionId, Engine, LocalPort, Password, ResolvedConnection, Source};
use spelaion_core::plugins::external::{
    AddError, ExternalState, MANIFEST_FILE, Origin, TheirPlugins,
};
use spelaion_core::plugins::fetch::{Body, FetchError, Transport, TransportError};
use spelaion_core::plugins::installed::{AppBuild, PluginStore};
use spelaion_core::plugins::pins::{platform, sha256_of};
use spelaion_core::{Engines, OpenError, OpenOptions, ResultStore, TunnelPool, open_connection};
use spelaion_driver_api::ConnectParams;
use spelaion_ssh::{KnownHosts, NonInteractive};

const PROGRAM: &str = env!("CARGO_BIN_EXE_spelaion-test-plugin");
/// The plugin's name, and the engine it adds (the test plugin serves `test`).
const NAME: &str = "acme-testdb";
const ENGINE: &str = "test";

fn program_name() -> String {
    if cfg!(windows) {
        "acme-testdb-plugin.exe".into()
    } else {
        "acme-testdb-plugin".into()
    }
}

fn manifest_with(changes: &[(&str, &str)]) -> String {
    let mut fields: Vec<(String, String)> = vec![
        ("schema_version".into(), "1".into()),
        ("name".into(), format!("{NAME:?}")),
        ("version".into(), "\"1.0.0\"".into()),
        ("author".into(), "\"Acme Ltd.\"".into()),
        ("protocol".into(), "\"1.0\"".into()),
        ("platform".into(), format!("{:?}", platform().unwrap())),
        ("program".into(), format!("{:?}", program_name())),
        ("engine".into(), format!("{ENGINE:?}")),
        ("engine_label".into(), "\"Test DB\"".into()),
        ("dialect".into(), "\"postgres\"".into()),
    ];
    for (key, value) in changes {
        match fields.iter_mut().find(|(k, _)| k == key) {
            Some(field) => field.1 = value.to_string(),
            None => fields.push((key.to_string(), value.to_string())),
        }
    }
    fields
        .into_iter()
        .map(|(key, value)| format!("{key} = {value}\n"))
        .collect()
}

/// A personal folder of its own, removed at the end.
struct Folder {
    root: PathBuf,
}

impl Folder {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("spelaion-theirs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        Self { root }
    }

    fn store(&self) -> PluginStore {
        PluginStore::at(self.root.join("plugins"))
    }

    fn theirs(&self) -> TheirPlugins {
        self.store().theirs()
    }

    /// An archive as a plugin's author packs it: its manifest and its program at the top.
    fn archive(&self, file: &str, manifest: &str) -> PathBuf {
        let program = std::fs::read(PROGRAM).unwrap();
        self.archive_of(
            file,
            &[
                (MANIFEST_FILE, manifest.as_bytes().to_vec()),
                (&program_name(), program),
                ("LICENSE", b"their licence".to_vec()),
            ],
        )
    }

    fn archive_of(&self, file: &str, members: &[(&str, Vec<u8>)]) -> PathBuf {
        let path = self.root.join("downloads").join(file);
        let packed = flate2::write::GzEncoder::new(
            std::fs::File::create(&path).unwrap(),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(packed);
        for (name, bytes) in members {
            let mut header = tar::Header::new_gnu();
            header.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::Regular);
            header.set_size(bytes.len() as u64);
            header.set_cksum();
            tar.append(&header, bytes.as_slice()).unwrap();
        }
        tar.into_inner().unwrap().finish().unwrap().flush().unwrap();
        path
    }

    /// Every file under the store, but the archives waiting for an answer.
    fn unpacked(&self) -> Vec<PathBuf> {
        fn walk(folder: &Path, out: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(folder) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, out);
                } else {
                    out.push(path);
                }
            }
        }
        let mut out = Vec::new();
        walk(&self.root.join("plugins"), &mut out);
        out.retain(|path| !path.components().any(|part| part.as_os_str() == ".pending"));
        out
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn app() -> AppBuild {
    AppBuild::this()
}

/// A personal connection of the engine `engine` of someone else's plugin.
fn connection(engine: &str, database: &str) -> ResolvedConnection {
    ResolvedConnection {
        id: ConnectionId {
            source: Source::Workspace,
            path: "team/testdb.toml".into(),
        },
        name: "Their engine".into(),
        engine: Engine::External,
        external_engine: Some(engine.into()),
        env: None,
        kind: None,
        params: ConnectParams {
            host: "db.internal".into(),
            port: 5432,
            user: "app".into(),
            database: database.into(),
            ..Default::default()
        },
        password: Password::None,
        tunnel: None,
        kubernetes: None,
        sqlite3: None,
        file_folder: None,
        databases: None,
        other_databases: spelaion_config::OtherDatabases::None,
        local_port: LocalPort::Any,
        keepalive: None,
        statement_timeout: None,
        session: Default::default(),
        mcp: Default::default(),
        masking: Default::default(),
    }
}

async fn open(
    engines: &Engines,
    resolved: ResolvedConnection,
) -> Result<spelaion_core::OpenConnection, OpenError> {
    tokio::time::timeout(
        Duration::from_secs(30),
        open_connection(
            resolved,
            engines,
            OpenOptions {
                tunnels: Arc::new(TunnelPool::new()),
                known_hosts: KnownHosts {
                    team_files: Vec::new(),
                    user_file: None,
                    app_file: std::env::temp_dir().join("unused-theirs-known-hosts"),
                },
                interaction: Arc::new(NonInteractive),
                password: None,
                read_only_session: false,
            },
            Arc::new(ResultStore::new()),
        ),
    )
    .await
    .expect("it took too long")
}

/// Added from a file: the dialog's facts come first, with nothing unpacked or run; then the
/// user's yes adds it, and a connection of its engine connects and browses, read under the name
/// `external`.
#[tokio::test]
async fn a_plugin_added_from_a_file_connects_and_browses() {
    let folder = Folder::new("file");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));

    let pending = theirs.inspect_file(&archive, &app()).unwrap();
    assert_eq!(pending.manifest.name, NAME);
    assert_eq!(pending.manifest.author, "Acme Ltd.");
    assert_eq!(pending.manifest.engine, ENGINE);
    assert_eq!(pending.archive_sha256, sha256_of(&archive).unwrap());
    assert_eq!(
        pending.program_sha256,
        sha256_of(Path::new(PROGRAM)).unwrap()
    );
    assert_eq!(pending.replaces, None);
    assert_eq!(
        pending.origin,
        Origin::File {
            path: archive.display().to_string()
        }
    );
    // Nothing of it is unpacked, and it is no engine yet.
    assert!(folder.unpacked().is_empty(), "{:?}", folder.unpacked());
    assert!(theirs.plugins(&app()).is_empty());
    let engines = Engines::builtin_with(&folder.store());
    assert!(engines.external(ENGINE).is_none());

    let added = theirs.add(&pending.archive_sha256, &app()).await.unwrap();
    assert_eq!(added.state, ExternalState::Trusted);
    assert_eq!(
        added.source.as_deref(),
        Some(archive.display().to_string().as_str())
    );

    let engines = Engines::builtin_with(&folder.store());
    let engine = engines.external(ENGINE).expect("its engine");
    assert_eq!(engine.dialect.name(), "external");
    let open = open(&engines, connection(ENGINE, "made")).await.unwrap();
    assert_eq!(open.session.dialect().name(), "external");
    let schemas = open.session.connection().list_schemas().await.unwrap();
    assert!(
        schemas.iter().any(|schema| schema.name == "main"),
        "{schemas:?}"
    );
    let objects = open
        .session
        .connection()
        .list_objects("main")
        .await
        .unwrap();
    assert!(!objects.is_empty());
    let table = open
        .session
        .connection()
        .describe_table("main", "people")
        .await
        .unwrap();
    assert!(!table.columns.is_empty());
    drop(open);

    // Removed with its pin: its engine is gone, and so is its folder.
    theirs.remove(NAME).await.unwrap();
    assert!(theirs.plugins(&app()).is_empty());
    assert!(folder.unpacked().is_empty());
    assert!(
        Engines::builtin_with(&folder.store())
            .external(ENGINE)
            .is_none()
    );
}

/// A made-up network: each address answers with its bytes, from where it says.
#[derive(Default)]
struct Network {
    pages: Mutex<HashMap<String, (String, Vec<u8>)>>,
    asked: Mutex<Vec<String>>,
}

impl Network {
    fn serve(&self, address: &str, bytes: Vec<u8>) {
        self.serve_from(address, address, bytes);
    }

    /// `address` answers, after redirects, from `from`.
    fn serve_from(&self, address: &str, from: &str, bytes: Vec<u8>) {
        self.pages
            .lock()
            .unwrap()
            .insert(address.into(), (from.into(), bytes));
    }

    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl Transport for Network {
    async fn get(&self, address: &str) -> Result<Box<dyn Body>, TransportError> {
        self.asked.lock().unwrap().push(address.to_string());
        let page = self.pages.lock().unwrap().get(address).cloned();
        let (from, bytes) = page.ok_or_else(|| TransportError("the host did not answer".into()))?;
        Ok(Box::new(Arriving {
            from,
            pieces: bytes.chunks(1 << 16).map(<[u8]>::to_vec).collect(),
        }))
    }
}

struct Arriving {
    from: String,
    pieces: std::collections::VecDeque<Vec<u8>>,
}

#[async_trait::async_trait]
impl Body for Arriving {
    fn address(&self) -> String {
        self.from.clone()
    }

    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        Ok(self.pieces.pop_front())
    }
}

fn release(archive_address: &str, archive: &Path) -> String {
    format!(
        "schema_version = 1\nname = {NAME:?}\nversion = \"1.0.0\"\n\n[platform.{}]\narchive = \
         {archive_address:?}\narchive_sha256 = {:?}\narchive_size = {}\n",
        platform().unwrap(),
        sha256_of(archive).unwrap(),
        std::fs::metadata(archive).unwrap().len()
    )
}

/// Added from an address: its archive, or its release manifest naming one. The dialog shows the
/// whole address, its host and the hash of what arrived; the address is read once, when asked,
/// and never again.
#[tokio::test]
async fn a_plugin_added_from_an_address_is_read_once() {
    let folder = Folder::new("address");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));
    let bytes = std::fs::read(&archive).unwrap();
    let network = Network::default();

    // The archive itself, which arrives from elsewhere after a redirect.
    let address = "https://downloads.acme.example/testdb/acme-testdb.tar.gz";
    network.serve_from(address, "https://cdn.acme.example/blob/123", bytes.clone());
    let pending = theirs
        .inspect_address(&network, &format!("  {address}\n"), &app())
        .await
        .unwrap();
    assert_eq!(pending.archive_sha256, sha256_of(&archive).unwrap());
    assert_eq!(
        pending.origin,
        Origin::Address {
            address: address.into(),
            host: "downloads.acme.example".into(),
            archive: None,
            archive_host: None,
            arrived_from: Some("https://cdn.acme.example/blob/123".into()),
        }
    );
    assert!(folder.unpacked().is_empty());
    theirs.discard(&pending.archive_sha256);
    assert!(theirs.add(&pending.archive_sha256, &app()).await.is_err());

    // Its release manifest, which names the archive for each platform.
    let manifest_address = "https://acme.example/testdb/release.toml";
    let archive_address = "https://github.example/acme/testdb/releases/v1.0.0/acme-testdb.tar.gz";
    network.serve(
        manifest_address,
        release(archive_address, &archive).into_bytes(),
    );
    network.serve(archive_address, bytes.clone());
    let pending = theirs
        .inspect_address(&network, manifest_address, &app())
        .await
        .unwrap();
    assert_eq!(
        pending.origin,
        Origin::Address {
            address: manifest_address.into(),
            host: "acme.example".into(),
            archive: Some(archive_address.into()),
            archive_host: Some("github.example".into()),
            arrived_from: None,
        }
    );
    let added = theirs.add(&pending.archive_sha256, &app()).await.unwrap();
    assert_eq!(added.source.as_deref(), Some(manifest_address));

    // From here nothing goes back to the address: not listing, not the engines, not a
    // connection.
    let asked = network.asked().len();
    let _ = theirs.plugins(&app());
    let engines = Engines::builtin_with(&folder.store());
    let open = open(&engines, connection(ENGINE, "made")).await.unwrap();
    drop(open);
    assert_eq!(network.asked().len(), asked, "{:?}", network.asked());
}

/// An address that is not HTTPS, or that redirects away from it, is not read; neither is an
/// archive its release manifest names over plain HTTP.
#[tokio::test]
async fn only_https_is_read_and_a_redirect_cannot_leave_it() {
    let folder = Folder::new("https");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));
    let bytes = std::fs::read(&archive).unwrap();
    let network = Network::default();

    for address in [
        "http://acme.example/acme-testdb.tar.gz",
        "file:///C:/acme-testdb.tar.gz",
        "ftp://acme.example/acme-testdb.tar.gz",
        "https://user:secret@acme.example/acme-testdb.tar.gz",
        "spelaiondb://plugins/add?from=https://acme.example/acme-testdb.tar.gz",
    ] {
        network.serve(address, bytes.clone());
        let error = theirs
            .inspect_address(&network, address, &app())
            .await
            .unwrap_err();
        assert!(matches!(error, AddError::Address(_)), "{address}: {error}");
    }
    assert!(network.asked().is_empty(), "{:?}", network.asked());

    // A redirect to plain HTTP: nothing it sent is kept.
    let address = "https://acme.example/acme-testdb.tar.gz";
    network.serve_from(
        address,
        "http://acme.example/acme-testdb.tar.gz",
        bytes.clone(),
    );
    let error = theirs
        .inspect_address(&network, address, &app())
        .await
        .unwrap_err();
    assert!(
        matches!(error, AddError::Fetch(FetchError::NotHttps { .. })),
        "{error}"
    );

    // A release manifest that names an archive over plain HTTP: the archive is not fetched.
    let manifest_address = "https://acme.example/release.toml";
    let plain = "http://acme.example/acme-testdb.tar.gz";
    network.serve(manifest_address, release(plain, &archive).into_bytes());
    let error = theirs
        .inspect_address(&network, manifest_address, &app())
        .await
        .unwrap_err();
    assert!(matches!(error, AddError::Address(_)), "{error}");
    assert!(!network.asked().iter().any(|asked| asked == plain));
    assert!(folder.unpacked().is_empty());
    assert!(theirs.plugins(&app()).is_empty());
}

/// A download larger than its bound is stopped, and one that is not what its release manifest
/// says is refused.
#[tokio::test]
async fn a_download_larger_than_its_bound_is_stopped() {
    let folder = Folder::new("bound");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));
    let mut bytes = std::fs::read(&archive).unwrap();
    let network = Network::default();
    let manifest_address = "https://acme.example/release.toml";
    let archive_address = "https://acme.example/acme-testdb.tar.gz";
    network.serve(
        manifest_address,
        release(archive_address, &archive).into_bytes(),
    );

    // More bytes than the release manifest said.
    bytes.extend_from_slice(&[0; 4096]);
    network.serve(archive_address, bytes.clone());
    let error = theirs
        .inspect_address(&network, manifest_address, &app())
        .await
        .unwrap_err();
    assert!(
        matches!(error, AddError::Fetch(FetchError::TooLarge { .. })),
        "{error}"
    );

    // As many bytes, but others: its hash is not the manifest's.
    let mut other = std::fs::read(&archive).unwrap();
    let last = other.len() - 1;
    other[last] ^= 0xff;
    network.serve(archive_address, other);
    let error = theirs
        .inspect_address(&network, manifest_address, &app())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SHA-256"), "{error}");

    // A release manifest that announces an archive above the bound is refused before it.
    let huge = release(archive_address, &archive).replace(
        &format!(
            "archive_size = {}",
            std::fs::metadata(&archive).unwrap().len()
        ),
        "archive_size = 999999999999",
    );
    network.serve(manifest_address, huge.into_bytes());
    let before = network.asked().len();
    assert!(
        theirs
            .inspect_address(&network, manifest_address, &app())
            .await
            .is_err()
    );
    assert_eq!(
        network.asked().len(),
        before + 1,
        "the archive was asked for"
    );

    // An address whose answer is neither an archive nor a release manifest.
    let page = "https://acme.example/download.html";
    network.serve(page, b"<html>Download our plugin!</html>".to_vec());
    assert!(
        theirs
            .inspect_address(&network, page, &app())
            .await
            .is_err()
    );
    assert!(folder.unpacked().is_empty());
}

/// The file the dialog showed is the one added, or none: an archive swapped while the dialog
/// was open is refused, and so is a program that changed after it was added, until the user
/// trusts it again.
#[tokio::test]
async fn a_swapped_file_is_refused() {
    let folder = Folder::new("swapped");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));
    let pending = theirs.inspect_file(&archive, &app()).unwrap();

    // Swapped while the dialog was open.
    let waiting = theirs
        .folder()
        .join(".pending")
        .join(format!("{}.tar.gz", pending.archive_sha256));
    let other = folder.archive("other.tar.gz", &manifest_with(&[("version", "\"6.6.6\"")]));
    std::fs::copy(&other, &waiting).unwrap();
    let error = theirs
        .add(&pending.archive_sha256, &app())
        .await
        .unwrap_err();
    assert!(matches!(error, AddError::Swapped { .. }), "{error}");
    assert!(theirs.plugins(&app()).is_empty());
    assert!(folder.unpacked().is_empty());
    // A hash the dialog never showed adds nothing.
    for made_up in ["../../evil", "0", &"a".repeat(64)] {
        assert!(theirs.add(made_up, &app()).await.is_err(), "{made_up}");
    }

    // Added, then its program changes on disk after the engines were set up.
    let pending = theirs.inspect_file(&archive, &app()).unwrap();
    let added = theirs.add(&pending.archive_sha256, &app()).await.unwrap();
    let engines = Engines::builtin_with(&folder.store());
    let mut program = std::fs::read(&added.program).unwrap();
    program.extend_from_slice(b"appended");
    std::fs::write(&added.program, &program).unwrap();
    let error = open(&engines, connection(ENGINE, "made"))
        .await
        .err()
        .expect("a changed program is not started");
    let message = error.to_string();
    assert!(message.contains("not part of SpelaionDB"), "{message}");
    assert!(message.contains(NAME), "{message}");

    // The page sees it changed, and the engines say why without starting it.
    let listed = theirs.plugins(&app());
    let found = match &listed[0].state {
        ExternalState::Changed { found } => found.clone(),
        other => panic!("{other:?}"),
    };
    let engines = Engines::builtin_with(&folder.store());
    let message = open(&engines, connection(ENGINE, "made"))
        .await
        .err()
        .expect("still refused")
        .to_string();
    assert!(message.contains("trust it again"), "{message}");

    // Trusting a hash that is not the file's now trusts nothing.
    let error = theirs
        .trust_again(NAME, &"0".repeat(64), &app())
        .unwrap_err();
    assert!(matches!(error, AddError::ChangedAgain { .. }), "{error}");
    // The user's yes to what they were shown: it is started again.
    theirs.trust_again(NAME, &found, &app()).unwrap();
    assert_eq!(theirs.plugins(&app())[0].state, ExternalState::Trusted);
}

/// Its errors name it and say it is not part of SpelaionDB, so a report goes to its author;
/// the connection test's diagnostics say so too.
#[tokio::test]
async fn its_errors_say_whose_it_is() {
    let folder = Folder::new("errors");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));
    let pending = theirs.inspect_file(&archive, &app()).unwrap();
    theirs.add(&pending.archive_sha256, &app()).await.unwrap();
    let engines = Engines::builtin_with(&folder.store());

    // The test plugin refuses the database `refuse`.
    let message = open(&engines, connection(ENGINE, "refuse"))
        .await
        .err()
        .expect("refused")
        .to_string();
    assert!(message.contains("no database for app"), "{message}");
    assert!(message.contains("acme-testdb 1.0.0"), "{message}");
    assert!(message.contains("Acme Ltd."), "{message}");
    assert!(message.contains("not part of SpelaionDB"), "{message}");

    let note = engines
        .engine_note(&connection(ENGINE, "made"))
        .expect("a note");
    assert!(note.contains("not part of SpelaionDB"), "{note}");
    assert!(note.contains(NAME), "{note}");
    // Ours have none.
    let mut ours = connection(ENGINE, "made");
    ours.engine = Engine::Sqlite;
    ours.external_engine = None;
    assert_eq!(engines.engine_note(&ours), None);
}

/// A team connection whose engine no plugin the user added serves says so and stops: nothing
/// is fetched or offered.
#[tokio::test]
async fn a_connection_whose_plugin_the_user_lacks_says_so_and_stops() {
    let folder = Folder::new("lacks");
    let engines = Engines::builtin_with(&folder.store());
    let error = open(&engines, connection("acme-otherdb", "made"))
        .await
        .err()
        .expect("not opened");
    assert!(
        matches!(&error, OpenError::ExternalEngineMissing { engine } if engine == "acme-otherdb"),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("not part of SpelaionDB"), "{message}");
    assert!(!message.contains("http"), "{message}");
    assert!(folder.unpacked().is_empty());
}

/// Agents get nothing on its engine: the program that serves them never starts one of other
/// people's plugins, and its connections are never visible to them, whatever `[mcp]` says.
#[tokio::test]
async fn agents_get_nothing_on_its_engine() {
    let folder = Folder::new("agents");
    let theirs = folder.theirs();
    let archive = folder.archive("acme-testdb.tar.gz", &manifest_with(&[]));
    let pending = theirs.inspect_file(&archive, &app()).unwrap();
    theirs.add(&pending.archive_sha256, &app()).await.unwrap();

    assert!(
        Engines::builtin_with(&folder.store())
            .external(ENGINE)
            .is_some()
    );
    let agents = Engines::builtin_for_agents(&folder.store());
    assert!(agents.external(ENGINE).is_none());
    let error = open(&agents, connection(ENGINE, "made"))
        .await
        .err()
        .unwrap();
    assert!(
        matches!(error, OpenError::ExternalEngineMissing { .. }),
        "{error}"
    );

    // No read barrier of ours: no read-only mode, and no agents.
    assert!(!Engine::External.has_read_barrier());
    assert!(!spelaion_core::has_read_barrier(Engine::External));
}

/// One plugin per engine: another one's engine is not taken over by a file, and a plugin that
/// passes for one of ours or names an engine of ours is refused before the dialog.
#[tokio::test]
async fn a_plugin_cannot_take_an_engine_over() {
    let folder = Folder::new("takeover");
    let theirs = folder.theirs();
    let first = folder.archive("first.tar.gz", &manifest_with(&[]));
    let pending = theirs.inspect_file(&first, &app()).unwrap();
    theirs.add(&pending.archive_sha256, &app()).await.unwrap();

    // Another plugin for the same engine.
    let other = folder.archive(
        "other.tar.gz",
        &manifest_with(&[("name", "\"evil-testdb\"")]),
    );
    let error = theirs.inspect_file(&other, &app()).unwrap_err();
    assert!(matches!(error, AddError::EngineTaken { .. }), "{error}");

    // A newer version of the same plugin replaces it, with the dialog again.
    let newer = folder.archive("newer.tar.gz", &manifest_with(&[("version", "\"1.1.0\"")]));
    let pending = theirs.inspect_file(&newer, &app()).unwrap();
    assert_eq!(pending.replaces.as_deref(), Some("1.0.0"));
    theirs.add(&pending.archive_sha256, &app()).await.unwrap();
    let listed = theirs.plugins(&app());
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].manifest.as_ref().unwrap().version, "1.1.0");

    for (change, why) in [
        (("engine", "\"postgres\""), "SpelaionDB's own"),
        (("name", "\"spelaiondb-plugin-postgres\""), "spelaion"),
        (("dialect", "\"klingon\""), "not one of SpelaionDB's"),
        (("protocol", "\"2.0\""), "plugin protocol"),
    ] {
        let archive = folder.archive("bad.tar.gz", &manifest_with(&[change]));
        let error = theirs
            .inspect_file(&archive, &app())
            .unwrap_err()
            .to_string();
        assert!(error.contains(why), "{change:?}: {error}");
    }
}

/// What is not a plugin's archive is refused before the dialog: no manifest, a program it does
/// not hold, a path that climbs out, a program that is not a file.
#[tokio::test]
async fn what_is_not_a_plugins_archive_is_refused() {
    let folder = Folder::new("archives");
    let theirs = folder.theirs();
    let program = std::fs::read(PROGRAM).unwrap();
    let manifest = manifest_with(&[]).into_bytes();
    for (name, members) in [
        (
            "no-manifest.tar.gz",
            vec![(program_name(), program.clone())],
        ),
        (
            "no-program.tar.gz",
            vec![(MANIFEST_FILE.to_string(), manifest.clone())],
        ),
        (
            "climbs.tar.gz",
            vec![
                (MANIFEST_FILE.to_string(), manifest.clone()),
                (format!("../{}", program_name()), program.clone()),
            ],
        ),
        (
            "twice.tar.gz",
            vec![
                (MANIFEST_FILE.to_string(), manifest.clone()),
                (program_name(), program.clone()),
                (program_name(), b"another".to_vec()),
            ],
        ),
    ] {
        let members: Vec<(&str, Vec<u8>)> = members
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.clone()))
            .collect();
        let archive = folder.archive_of(name, &members);
        assert!(theirs.inspect_file(&archive, &app()).is_err(), "{name}");
    }
    // Not a .tar.gz at all.
    let plain = folder.root.join("downloads").join("plugin.exe");
    std::fs::copy(PROGRAM, &plain).unwrap();
    assert!(theirs.inspect_file(&plain, &app()).is_err());
    assert!(folder.unpacked().is_empty());
}
