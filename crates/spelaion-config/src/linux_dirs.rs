//! Where a client installed on Linux keeps its files, for the importers' default search: the
//! native folders, and those of its Flatpak and Snap packages, which many Linux users install
//! instead. Each reader lists its own candidates (native first, each package path with the
//! source it was checked against); this module only knows the layout every package shares.

use std::path::{Path, PathBuf};

/// The user's home and base directories, as a native Linux app sees them.
#[derive(Debug, Clone)]
pub(crate) struct LinuxHome {
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, or `~/.config`.
    pub config: PathBuf,
    /// `$XDG_DATA_HOME`, or `~/.local/share`.
    pub data: PathBuf,
}

impl LinuxHome {
    /// This user's, from `HOME` and the XDG variables; `None` without a home.
    pub fn from_env() -> Option<Self> {
        let home = PathBuf::from(std::env::var_os("HOME")?);
        let base = |var: &str, default: &str| {
            std::env::var_os(var)
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(default))
        };
        Some(LinuxHome {
            config: base("XDG_CONFIG_HOME", ".config"),
            data: base("XDG_DATA_HOME", ".local/share"),
            home,
        })
    }

    /// A home whose base directories are the XDG defaults under it (tests use a fake one).
    #[cfg(test)]
    pub fn at(home: &Path) -> Self {
        LinuxHome {
            home: home.to_path_buf(),
            config: home.join(".config"),
            data: home.join(".local/share"),
        }
    }

    /// The folder a Flatpak app keeps its own files in: `~/.var/app/<app id>`. Inside the
    /// sandbox `XDG_CONFIG_HOME` is its `config` folder and `XDG_DATA_HOME` its `data` folder,
    /// while `HOME` stays the real home
    /// (<https://docs.flatpak.org/en/latest/conventions.html#xdg-base-directories>).
    pub fn flatpak(&self, app_id: &str) -> PathBuf {
        self.home.join(".var").join("app").join(app_id)
    }

    /// A snap's own folder for its current revision, `~/snap/<name>/current`: `SNAP_USER_DATA` is
    /// `~/snap/<name>/<revision>`, and snapd keeps `current` pointing at the revision in use. A
    /// strictly confined snap gets it as its `HOME` (snapd's `snap/snapenv/snapenv.go`:
    /// <https://github.com/canonical/snapd/blob/master/snap/snapenv/snapenv.go>).
    pub fn snap(&self, name: &str) -> PathBuf {
        self.home.join("snap").join(name).join("current")
    }
}

/// The first candidate where `found` sees the client's files, or the first candidate (the
/// native folder, named in the "nothing found" message) when none has them.
pub(crate) fn first_found(
    candidates: Vec<PathBuf>,
    found: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates
        .iter()
        .find(|dir| found(dir))
        .or(candidates.first())
        .cloned()
}

/// Fake homes laid out as each install writes them, for the readers' tests.
#[cfg(test)]
pub(crate) mod testing {
    use std::path::PathBuf;

    /// An empty home for one test.
    pub fn fake_home(name: &str) -> PathBuf {
        let home =
            std::env::temp_dir().join(format!("spelaion-home-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    /// The folder a strictly confined snap writes in at revision `revision`
    /// (`~/snap/<name>/<revision>`), with `current` linking to it as snapd lays it out.
    #[cfg(unix)]
    pub fn snap_revision(home: &std::path::Path, name: &str, revision: &str) -> PathBuf {
        let snap = home.join("snap").join(name);
        std::fs::create_dir_all(snap.join(revision)).unwrap();
        std::os::unix::fs::symlink(revision, snap.join("current")).unwrap();
        snap.join(revision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_found_prefers_the_first_candidate_with_files_and_else_the_first() {
        let root = std::env::temp_dir().join(format!("spelaion-linux-dirs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (a, b, c) = (root.join("a"), root.join("b"), root.join("c"));
        std::fs::create_dir_all(&b).unwrap();
        std::fs::create_dir_all(&c).unwrap();
        let all = vec![a.clone(), b.clone(), c.clone()];
        assert_eq!(first_found(all.clone(), |p| p.is_dir()), Some(b));
        assert_eq!(first_found(all, |_| false), Some(a));
        assert_eq!(first_found(Vec::new(), |_| true), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn package_homes_follow_flatpak_and_snap_layouts() {
        let home = LinuxHome::at(Path::new("/home/u"));
        assert_eq!(
            home.flatpak("org.example.App"),
            Path::new("/home/u/.var/app/org.example.App")
        );
        assert_eq!(
            home.snap("example"),
            Path::new("/home/u/snap/example/current")
        );
    }
}
