//! Where each client keeps its connections on this computer, how the import reads them, and
//! what the import's list shows for each client before any is picked (SDB-446, ADR-039).
//!
//! The desktop's import is a thin adapter over this module: the folder or file a client is read
//! from (the user's choice, the `SPELAION_<CLIENT>_DIR` override the smoke test sets so it never
//! reads the user's own clients, or where the client keeps it on this system), the reader of that
//! client, and the errors the import tells apart (an encrypted export without its password, a
//! client that keeps nothing this system reads on its own).
//!
//! [`import_source`] counts what a client has without reading any saved password
//! ([`SavedSecrets::Skip`]): no credentials file is decrypted, no keychain is asked, no encrypted
//! export is opened.

use std::path::{Path, PathBuf};

use spelaion_driver_api::Secret;

use crate::import::{ImportClient, ImportedConnection, SavedSecrets};
use crate::{
    BeekeeperError, DatagripError, DbeaverError, HeidiSqlError, PgadminError, TableplusError,
    WorkbenchError, WorkbenchPasswords,
};

/// Every client, in the order the list shows them before any count is known.
pub const IMPORT_CLIENTS: [ImportClient; 7] = [
    ImportClient::Dbeaver,
    ImportClient::Datagrip,
    ImportClient::Tableplus,
    ImportClient::Heidisql,
    ImportClient::Workbench,
    ImportClient::Pgadmin,
    ImportClient::Beekeeper,
];

/// Where each client is read from when the user chose no folder or file: the folder or file of
/// each client (which may not exist), and whether HeidiSQL's sessions are read from Windows'
/// registry. [`ImportPlaces::this_system`] is the app's; a test builds its own.
#[derive(Debug, Clone, Default)]
pub struct ImportPlaces {
    dbeaver: Option<PathBuf>,
    /// JetBrains' configuration folder, which holds DataGrip's.
    jetbrains: Option<PathBuf>,
    tableplus: Option<PathBuf>,
    heidisql: Option<PathBuf>,
    workbench: Option<PathBuf>,
    pgadmin: Option<PathBuf>,
    beekeeper: Option<PathBuf>,
    heidisql_registry: bool,
}

/// An environment override of a client's folder (the smoke test points them at its own files).
fn overridden(variable: &str) -> Option<PathBuf> {
    std::env::var_os(variable).map(PathBuf::from)
}

impl ImportPlaces {
    /// Where the clients keep their connections on this system, each overridden by its
    /// `SPELAION_<CLIENT>_DIR` (`SPELAION_HEIDISQL_FILE` for HeidiSQL's export) so the smoke test
    /// never reads the user's own clients. HeidiSQL's registry is read only on Windows, only
    /// without that file, and never with `SPELAION_HEIDISQL_REGISTRY=off`.
    pub fn this_system() -> Self {
        let heidisql = overridden("SPELAION_HEIDISQL_FILE");
        let registry_off =
            std::env::var_os("SPELAION_HEIDISQL_REGISTRY").is_some_and(|v| v == "off");
        Self {
            dbeaver: overridden("SPELAION_DBEAVER_DIR").or_else(crate::dbeaver_workspace),
            jetbrains: overridden("SPELAION_JETBRAINS_DIR").or_else(crate::jetbrains_dir),
            tableplus: overridden("SPELAION_TABLEPLUS_DIR").or_else(crate::tableplus_dir),
            heidisql_registry: heidisql.is_none()
                && !registry_off
                && crate::heidisql_has_registry(),
            heidisql,
            workbench: overridden("SPELAION_WORKBENCH_DIR").or_else(crate::workbench_dir),
            pgadmin: overridden("SPELAION_PGADMIN_DIR").or_else(crate::pgadmin_dir),
            beekeeper: overridden("SPELAION_BEEKEEPER_DIR").or_else(crate::beekeeper_dir),
        }
    }

    /// No place for any client, and no registry: each is read only from what is chosen. Tests
    /// add the places they mean with [`ImportPlaces::with`].
    pub fn none() -> Self {
        Self::default()
    }

    /// The same places, with `client` read from `place` (for DataGrip, JetBrains' folder).
    pub fn with(mut self, client: ImportClient, place: impl Into<PathBuf>) -> Self {
        let place = Some(place.into());
        match client {
            ImportClient::Dbeaver => self.dbeaver = place,
            ImportClient::Datagrip => self.jetbrains = place,
            ImportClient::Tableplus => self.tableplus = place,
            ImportClient::Heidisql => self.heidisql = place,
            ImportClient::Workbench => self.workbench = place,
            ImportClient::Pgadmin => self.pgadmin = place,
            ImportClient::Beekeeper => self.beekeeper = place,
        }
        self
    }

    /// The folder or file a client is read from when nothing is chosen. `None` for HeidiSQL
    /// without an export file (its registry, or nothing outside Windows).
    pub fn place(&self, client: ImportClient) -> Option<&Path> {
        match client {
            ImportClient::Dbeaver => self.dbeaver.as_deref(),
            ImportClient::Datagrip => self.jetbrains.as_deref(),
            ImportClient::Tableplus => self.tableplus.as_deref(),
            ImportClient::Heidisql => self.heidisql.as_deref(),
            ImportClient::Workbench => self.workbench.as_deref(),
            ImportClient::Pgadmin => self.pgadmin.as_deref(),
            ImportClient::Beekeeper => self.beekeeper.as_deref(),
        }
    }

    /// Whether the client is read from somewhere without the user choosing a folder or file.
    pub fn has_default(&self, client: ImportClient) -> bool {
        self.place(client).is_some() || (client == ImportClient::Heidisql && self.heidisql_registry)
    }
}

/// Why a client's connections could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ImportReadError {
    /// An encrypted export, and no password was given for it.
    #[error("the file needs its export password")]
    NeedsPassword,
    /// The password given does not open the encrypted export.
    #[error("the password does not open the export file")]
    WrongPassword,
    /// Nothing was chosen and the client keeps nothing this system can read on its own
    /// (HeidiSQL outside Windows): the user must choose a folder or a file first.
    #[error("choose a folder or file to import from")]
    NoDefault,
    /// The client keeps nothing where it was looked for (not installed, never used, or a chosen
    /// folder without its files): the reader's own words.
    #[error("{0}")]
    NotFound(String),
    /// The client's files are there and could not be read: why.
    #[error("{0}")]
    Other(String),
}

/// What was read: where (to show it), and the connections.
pub struct ImportRead {
    pub place: String,
    pub connections: Vec<ImportedConnection>,
}

/// Reads a client's connections from the folder or file chosen, or from its place in `places`.
/// `password` opens an encrypted export (TablePlus's); `secrets` says whether passwords the
/// client saved in its files are read. Blocking: call it off the UI thread.
pub fn read_import(
    places: &ImportPlaces,
    client: ImportClient,
    chosen: Option<&Path>,
    password: Option<&Secret>,
    secrets: SavedSecrets,
) -> Result<ImportRead, ImportReadError> {
    let chosen = chosen
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf);
    let default_place = |client: ImportClient| places.place(client).map(Path::to_path_buf);
    let place = |client: ImportClient| match chosen.clone() {
        Some(path) => Ok(path),
        None => default_place(client).ok_or(ImportReadError::NoDefault),
    };
    let read = |path: PathBuf, connections| {
        Ok(ImportRead {
            place: path.display().to_string(),
            connections,
        })
    };
    match client {
        ImportClient::Dbeaver => {
            let dir = place(client)?;
            let found = crate::dbeaver::read_dbeaver_with(&dir, secrets).map_err(|e| match e {
                DbeaverError::NotFound(_) => ImportReadError::NotFound(e.to_string()),
                DbeaverError::Unreadable { .. } => other(e),
            })?;
            read(dir, found)
        }
        ImportClient::Datagrip => {
            let jetbrains = default_place(client);
            let config_dirs = jetbrains
                .as_deref()
                .map(crate::datagrip_dirs)
                .unwrap_or_default();
            let dir = match chosen.clone() {
                Some(dir) => dir,
                None => jetbrains.ok_or(ImportReadError::NoDefault)?,
            };
            let found = crate::read_datagrip(&dir, &config_dirs).map_err(|e| match e {
                DatagripError::NotFound(_) => ImportReadError::NotFound(e.to_string()),
                DatagripError::Unreadable { .. } => other(e),
            })?;
            read(dir, found)
        }
        ImportClient::Heidisql => {
            let file = match chosen.clone().or_else(|| default_place(client)) {
                Some(file) => file,
                None if places.heidisql_registry => {
                    let found = crate::heidisql::read_heidisql_registry_with(secrets).map_err(
                        |e| match e {
                            // No HeidiSQL sessions in the registry: HeidiSQL was never used here.
                            HeidiSqlError::Registry(_) => ImportReadError::NotFound(e.to_string()),
                            HeidiSqlError::RegistryUnavailable
                            | HeidiSqlError::Unreadable { .. }
                            | HeidiSqlError::Invalid(_) => other(e),
                        },
                    )?;
                    return read(PathBuf::from(REGISTRY), found);
                }
                None => return Err(ImportReadError::NoDefault),
            };
            if !file.exists() {
                return Err(ImportReadError::NotFound(format!(
                    "{} does not exist",
                    file.display()
                )));
            }
            let found = crate::heidisql::read_heidisql_with(&file, secrets).map_err(other)?;
            read(file, found)
        }
        ImportClient::Tableplus => {
            let dir = place(client)?;
            let found = crate::read_tableplus(&dir, password).map_err(|e| match e {
                TableplusError::NeedsPassword(_) => ImportReadError::NeedsPassword,
                TableplusError::WrongPassword(_) => ImportReadError::WrongPassword,
                TableplusError::NotFound(_) => ImportReadError::NotFound(e.to_string()),
                TableplusError::Unreadable { .. } => other(e),
            })?;
            read(dir, found)
        }
        ImportClient::Workbench => {
            let dir = place(client)?;
            let found = crate::read_workbench(&dir, WorkbenchPasswords::this_system()).map_err(
                |e| match e {
                    WorkbenchError::NotFound(_) => ImportReadError::NotFound(e.to_string()),
                    WorkbenchError::Unreadable { .. } => other(e),
                },
            )?;
            read(dir, found)
        }
        ImportClient::Pgadmin => {
            let path = place(client)?;
            let found = crate::read_pgadmin(&path).map_err(|e| match e {
                PgadminError::NotFound(_) => ImportReadError::NotFound(e.to_string()),
                PgadminError::Unreadable { .. } => other(e),
            })?;
            read(path, found)
        }
        ImportClient::Beekeeper => {
            let path = place(client)?;
            let found =
                crate::beekeeper::read_beekeeper_with(&path, secrets).map_err(|e| match e {
                    BeekeeperError::NotFound(_) => ImportReadError::NotFound(e.to_string()),
                    BeekeeperError::Unreadable { .. } => other(e),
                })?;
            read(path, found)
        }
    }
}

/// Where HeidiSQL keeps its sessions on Windows.
const REGISTRY: &str = r"HKCU\Software\HeidiSQL\Servers";

fn other(e: impl std::fmt::Display) -> ImportReadError {
    ImportReadError::Other(e.to_string())
}

/// What the import's list says of a client before it is picked (SDB-446). One state, read by
/// the page as it is (ADR-060).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportSourceState {
    /// Read where the client keeps its connections (or where the test pointed): how many.
    Found { place: String, count: usize },
    /// The client keeps nothing where it would on this computer (not installed, or never
    /// used): a folder or an export file can still be chosen.
    NotFound,
    /// Nothing this computer reads on its own: the connections come from a file exported from
    /// the client (HeidiSQL outside Windows).
    NeedsFile,
    /// An export encrypted with a password: it is counted once opened with it.
    Encrypted { place: String },
    /// The client's files are there and could not be read: why. The other clients are not held.
    Failed { reason: String },
}

/// What one client has on this computer: from `chosen` (a test's or the user's folder or file)
/// or its place in `places`, counted without reading any saved password (module docs). Blocking.
pub fn import_source(
    places: &ImportPlaces,
    client: ImportClient,
    chosen: Option<&Path>,
) -> ImportSourceState {
    match read_import(places, client, chosen, None, SavedSecrets::Skip) {
        Ok(read) => ImportSourceState::Found {
            place: read.place,
            count: read.connections.len(),
        },
        Err(ImportReadError::NotFound(_)) => ImportSourceState::NotFound,
        Err(ImportReadError::NoDefault) => ImportSourceState::NeedsFile,
        Err(ImportReadError::NeedsPassword | ImportReadError::WrongPassword) => {
            ImportSourceState::Encrypted {
                place: chosen
                    .map(|p| p.display().to_string())
                    .or_else(|| places.place(client).map(|p| p.display().to_string()))
                    .unwrap_or_default(),
            }
        }
        Err(ImportReadError::Other(reason)) => ImportSourceState::Failed { reason },
    }
}
