//! The folders the window holds open (ADR-075, SDB-500): an ordered list, at most
//! [`MAX_FOLDERS`], each with its own configuration (that folder loaded alone with the personal
//! layer, `spelaion_config::load`). No configuration ever holds two repositories: which layers a
//! connection sees, and what a personal connection takes from the open folders, is the
//! configuration crate's (`Resolver::with_open_folders`, `Config::connection_definition_open`).
//!
//! The page still shows one folder at a time: the last one opened ([`OpenFolders::shown`]), and
//! choosing a folder closes the others first. The list, its limit, the same root never twice
//! and what is remembered between runs are decided here, with their tests, so the card that
//! shows several folders only changes the page.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use spelaion_config::{Config, canonical_folder, load};

/// The most folders one window holds open (ADR-075 point 1).
pub const MAX_FOLDERS: usize = 10;

/// The file in the personal folder that remembers the open folders between runs.
const REMEMBERED_FILE: &str = "desktop-open-folders.json";
/// Where the one open folder was remembered before several could be: read once, to migrate.
const LEGACY_FILE: &str = "desktop-last-workspace.txt";

/// One open folder.
#[derive(Debug, Clone)]
pub struct OpenFolder {
    /// The path as it was chosen (what is remembered and shown in the recent folders).
    pub path: PathBuf,
    /// Its canonical root: the repository's when the path is inside one, else the path's. Two
    /// paths with the same root are one folder.
    pub root: PathBuf,
    /// This folder loaded alone with the personal layer, replaced whole on a reload.
    pub config: Arc<Config>,
    /// Tells this opening of the folder from a later one of the same root: a reload started
    /// before the folder was closed and opened again never lands in the new one.
    opened: u64,
}

/// Why a folder could not be added or closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FolderError {
    /// Already [`MAX_FOLDERS`] open.
    TooMany,
    /// The path is not a folder.
    NotAFolder(PathBuf),
    /// No open folder has this root.
    NotOpen(PathBuf),
}

impl std::fmt::Display for FolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FolderError::TooMany => write!(
                f,
                "{MAX_FOLDERS} folders are open already: close one before opening another"
            ),
            FolderError::NotAFolder(path) => write!(f, "{} is not a folder", path.display()),
            FolderError::NotOpen(root) => write!(f, "{} is not open", root.display()),
        }
    }
}

/// The open folders, in the order they were opened, and the personal layer alone for when none
/// is open.
#[derive(Debug)]
pub struct OpenFolders {
    user_dir: PathBuf,
    folders: Vec<OpenFolder>,
    personal: Arc<Config>,
    next_opening: u64,
}

/// The canonical root of a folder loaded from `path`.
fn root_of(path: &Path, config: &Config) -> PathBuf {
    canonical_folder(config.workspace_root.as_deref().unwrap_or(path))
}

impl OpenFolders {
    /// `paths` opened in this order (the remembered ones, or the one a launch names): those that
    /// are not folders any more are left out, a root already open is opened once, and no more
    /// than [`MAX_FOLDERS`].
    pub fn open(user_dir: &Path, paths: &[PathBuf]) -> Self {
        let mut folders = Self {
            user_dir: user_dir.to_path_buf(),
            folders: Vec::new(),
            personal: Arc::new(load(None, user_dir)),
            next_opening: 0,
        };
        for path in paths {
            // A remembered folder that is gone, or one past the limit, is left out.
            let _ = folders.add(path);
        }
        folders
    }

    /// Opens `path` after the others; nothing open is closed. The same root again is the folder
    /// already open (its index is returned).
    pub fn add(&mut self, path: &Path) -> Result<usize, FolderError> {
        if !path.is_dir() {
            return Err(FolderError::NotAFolder(path.to_path_buf()));
        }
        let config = load(Some(path), &self.user_dir);
        let root = root_of(path, &config);
        if let Some(index) = self.folders.iter().position(|f| f.root == root) {
            return Ok(index);
        }
        if self.folders.len() >= MAX_FOLDERS {
            return Err(FolderError::TooMany);
        }
        self.next_opening += 1;
        self.folders.push(OpenFolder {
            path: path.to_path_buf(),
            root,
            config: Arc::new(config),
            opened: self.next_opening,
        });
        Ok(self.folders.len() - 1)
    }

    /// Closes the folder of `root` (canonical, as listed) and returns it. The caller closes its
    /// sessions; the personal ones and other folders' stay.
    pub fn close(&mut self, root: &Path) -> Result<OpenFolder, FolderError> {
        let root = canonical_folder(root);
        let index = self
            .folders
            .iter()
            .position(|f| f.root == root)
            .ok_or_else(|| FolderError::NotOpen(root.clone()))?;
        Ok(self.folders.remove(index))
    }

    /// Every open folder, in the order they were opened.
    pub fn folders(&self) -> &[OpenFolder] {
        &self.folders
    }

    /// The configuration the page shows: the last folder opened, or the personal layer alone.
    pub fn shown(&self) -> Arc<Config> {
        self.folders
            .last()
            .map_or_else(|| self.personal.clone(), |f| f.config.clone())
    }

    /// Every open folder's configuration, in the order they were opened (what a personal
    /// connection reads, `Resolver::with_open_folders`).
    pub fn configs(&self) -> Vec<Arc<Config>> {
        self.folders.iter().map(|f| f.config.clone()).collect()
    }

    /// What to load again: each open folder's path and opening, read under the lock, loaded
    /// outside it and installed with [`OpenFolders::install`].
    pub fn to_reload(&self) -> Vec<(PathBuf, u64)> {
        self.folders
            .iter()
            .map(|f| (f.path.clone(), f.opened))
            .collect()
    }

    /// Installs configurations loaded for [`OpenFolders::to_reload`]: each only into the same
    /// opening of its folder (one closed, or closed and opened again, in the meantime is left as
    /// it is), and never into another folder. The personal layer alone is loaded again too.
    pub fn install(&mut self, loaded: Vec<(u64, Config)>, personal: Config) {
        for (opened, config) in loaded {
            if let Some(folder) = self.folders.iter_mut().find(|f| f.opened == opened) {
                folder.config = Arc::new(config);
            }
        }
        self.personal = Arc::new(personal);
    }

    /// The paths to remember, in order.
    pub fn paths(&self) -> Vec<PathBuf> {
        self.folders.iter().map(|f| f.path.clone()).collect()
    }
}

/// What the remembered file holds.
#[derive(Debug, Serialize, Deserialize)]
struct Remembered {
    schema_version: u32,
    folders: Vec<PathBuf>,
}

/// The folders open when the app last ran, in order. Without the file, the one folder of the
/// older single-path file, once (it is then remembered in the new file). Nothing: none.
pub fn remembered(user_dir: &Path) -> Vec<PathBuf> {
    let file = user_dir.join(REMEMBERED_FILE);
    if let Ok(text) = std::fs::read_to_string(&file) {
        return serde_json::from_str::<Remembered>(&text)
            .map(|r| r.folders)
            .unwrap_or_default();
    }
    let legacy = std::fs::read_to_string(user_dir.join(LEGACY_FILE))
        .ok()
        .map(|text| PathBuf::from(text.trim()))
        .filter(|path| !path.as_os_str().is_empty());
    let folders: Vec<PathBuf> = legacy.into_iter().collect();
    if !folders.is_empty() {
        let _ = remember(user_dir, &folders);
    }
    folders
}

/// Remembers the open folders for the next run: replaced whole (a crash never leaves half a
/// list) and owner-only when new, like the personal folder's other files.
pub fn remember(user_dir: &Path, folders: &[PathBuf]) -> std::io::Result<()> {
    let _ = std::fs::create_dir_all(user_dir);
    let text = serde_json::to_string_pretty(&Remembered {
        schema_version: 1,
        folders: folders.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    spelaion_config::atomic_file::replace(
        &user_dir.join(REMEMBERED_FILE),
        text.as_bytes(),
        spelaion_config::atomic_file::NewFileMode::OwnerOnly,
    )
    .map_err(|e| std::io::Error::other(e.to_string()))
}

/// The keys of the sessions that belong to the folder of `root`: those whose connection is that
/// folder's. Personal connections' sessions (no folder) and other folders' are never among them.
pub fn sessions_of<'a>(
    sessions: impl Iterator<Item = (&'a String, Option<&'a Path>)>,
    root: &Path,
) -> Vec<String> {
    sessions
        .filter(|(_, folder)| *folder == Some(root))
        .map(|(key, _)| key.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dirs {
        base: PathBuf,
    }

    impl Dirs {
        fn new(name: &str) -> Self {
            let base = std::env::temp_dir().join(format!(
                "spelaion-open-folders-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(base.join("user")).unwrap();
            Self { base }
        }

        fn user(&self) -> PathBuf {
            self.base.join("user")
        }

        /// A repository folder (it has a `.spelaion` folder).
        fn repo(&self, name: &str) -> PathBuf {
            let path = self.base.join(name);
            std::fs::create_dir_all(path.join(".spelaion")).unwrap();
            path
        }
    }

    impl Drop for Dirs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    #[test]
    fn adding_keeps_the_open_folders_and_closing_takes_out_only_that_one() {
        let dirs = Dirs::new("add-close");
        let (a, b) = (dirs.repo("a"), dirs.repo("b"));
        let mut open = OpenFolders::open(&dirs.user(), &[]);
        assert!(open.folders().is_empty());
        assert_eq!(
            open.shown().workspace_root,
            None,
            "the personal layer alone"
        );

        open.add(&a).unwrap();
        open.add(&b).unwrap();
        let roots: Vec<PathBuf> = open.folders().iter().map(|f| f.root.clone()).collect();
        assert_eq!(roots, [canonical_folder(&a), canonical_folder(&b)]);
        assert_eq!(open.configs().len(), 2);
        // The page shows the last one opened.
        assert_eq!(open.shown().workspace_root.as_deref(), Some(b.as_path()));

        let closed = open.close(&a).unwrap();
        assert_eq!(closed.root, canonical_folder(&a));
        assert_eq!(open.paths(), std::slice::from_ref(&b));
        assert_eq!(
            open.close(&a).unwrap_err(),
            FolderError::NotOpen(canonical_folder(&a))
        );
    }

    #[test]
    fn an_eleventh_folder_is_refused_with_a_message() {
        let dirs = Dirs::new("eleven");
        let mut open = OpenFolders::open(&dirs.user(), &[]);
        for i in 0..MAX_FOLDERS {
            open.add(&dirs.repo(&format!("r{i}"))).unwrap();
        }
        let refused = open.add(&dirs.repo("one-too-many")).unwrap_err();
        assert_eq!(refused, FolderError::TooMany);
        assert!(refused.to_string().contains("close one"), "{refused}");
        assert_eq!(open.folders().len(), MAX_FOLDERS);
        // A folder already open is still found.
        assert_eq!(open.add(&dirs.base.join("r3")).unwrap(), 3);
    }

    #[test]
    fn the_same_canonical_root_is_not_opened_twice() {
        let dirs = Dirs::new("twice");
        let a = dirs.repo("a");
        std::fs::create_dir_all(a.join("sub/deeper")).unwrap();
        let mut open = OpenFolders::open(&dirs.user(), &[]);
        assert_eq!(open.add(&a).unwrap(), 0);
        // Another spelling of it, and a folder inside the repository: the same folder.
        assert_eq!(open.add(&a.join("sub/../")).unwrap(), 0);
        assert_eq!(open.add(&a.join("sub/deeper")).unwrap(), 0);
        assert_eq!(open.folders().len(), 1);
        assert_eq!(
            open.add(&dirs.base.join("missing")).unwrap_err(),
            FolderError::NotAFolder(dirs.base.join("missing"))
        );
    }

    #[test]
    fn a_restart_opens_the_same_folders_and_an_old_single_folder_file_is_migrated() {
        let dirs = Dirs::new("remember");
        let (a, b) = (dirs.repo("a"), dirs.repo("b"));
        // Before several folders: one path in the old file.
        std::fs::write(dirs.user().join(LEGACY_FILE), format!("{}\n", a.display())).unwrap();
        assert_eq!(remembered(&dirs.user()), std::slice::from_ref(&a));
        assert!(dirs.user().join(REMEMBERED_FILE).is_file(), "migrated once");

        let mut open = OpenFolders::open(&dirs.user(), &remembered(&dirs.user()));
        open.add(&b).unwrap();
        remember(&dirs.user(), &open.paths()).unwrap();
        // The next run: the same folders in the same order, whatever the old file says.
        std::fs::write(dirs.user().join(LEGACY_FILE), "/elsewhere").unwrap();
        let again = OpenFolders::open(&dirs.user(), &remembered(&dirs.user()));
        assert_eq!(again.paths(), [a.clone(), b.clone()]);
        // A remembered folder that is gone is left out.
        std::fs::remove_dir_all(&a).unwrap();
        let without_a = OpenFolders::open(&dirs.user(), &remembered(&dirs.user()));
        assert_eq!(without_a.paths(), [b]);
    }

    #[test]
    fn a_restored_folder_is_trusted_or_not_as_it_was() {
        let dirs = Dirs::new("trust");
        let (a, b) = (dirs.repo("a"), dirs.repo("b"));
        let open = OpenFolders::open(&dirs.user(), &[a.clone(), b.clone()]);
        let mut trust = spelaion_config::TrustStore::load(&dirs.user()).unwrap();
        trust.trust(&open.folders()[0].config).unwrap();
        remember(&dirs.user(), &open.paths()).unwrap();

        let again = OpenFolders::open(&dirs.user(), &remembered(&dirs.user()));
        let trust = spelaion_config::TrustStore::load(&dirs.user()).unwrap();
        let trusted: Vec<bool> = again
            .folders()
            .iter()
            .map(|f| {
                f.config
                    .workspace_root
                    .as_deref()
                    .is_some_and(|root| trust.is_trusted(root))
            })
            .collect();
        assert_eq!(trusted, [true, false]);
    }

    #[test]
    fn a_reload_lands_only_in_the_same_opening_of_its_folder() {
        let dirs = Dirs::new("reload");
        let (a, b) = (dirs.repo("a"), dirs.repo("b"));
        let mut open = OpenFolders::open(&dirs.user(), &[a.clone(), b.clone()]);
        let pending = open.to_reload();
        // A is closed and opened again while its reload runs: that reload is stale.
        open.close(&a).unwrap();
        open.add(&a).unwrap();
        let (b_before, a_before) = (
            open.folders()[0].config.clone(),
            open.folders()[1].config.clone(),
        );
        let loaded: Vec<(u64, Config)> = pending
            .iter()
            .map(|(path, opened)| (*opened, load(Some(path), &dirs.user())))
            .collect();
        open.install(loaded, load(None, &dirs.user()));
        assert!(
            Arc::ptr_eq(&open.folders()[1].config, &a_before),
            "A's new opening kept"
        );
        assert!(
            !Arc::ptr_eq(&open.folders()[0].config, &b_before),
            "B reloaded"
        );
    }

    #[test]
    fn closing_a_folder_closes_its_sessions_only() {
        let (a, b) = (PathBuf::from("/work/a"), PathBuf::from("/work/b"));
        let sessions: Vec<(String, Option<PathBuf>)> = vec![
            ("tab-1".into(), Some(a.clone())),
            ("meta:a".into(), Some(a.clone())),
            ("tab-2".into(), None),
            ("tab-3".into(), Some(b.clone())),
        ];
        let keys = sessions_of(sessions.iter().map(|(k, f)| (k, f.as_deref())), &a);
        assert_eq!(keys, ["tab-1", "meta:a"]);
    }
}
