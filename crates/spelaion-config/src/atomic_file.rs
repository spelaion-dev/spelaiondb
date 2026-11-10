//! Replacing a file atomically, for every file the app rewrites in place: the editor's `.sql`
//! files (ADR-036) and the AI clients' MCP files (ADR-033).
//!
//! The new bytes go to a temporary file in the same folder, which is flushed and renamed over
//! the old one, so a crash leaves the old file or the new one, never part of one. On Unix the
//! new file keeps the old one's mode, and it is never readable by anyone else while it is
//! written: the temporary file is created owner-only and only then given the old mode. A
//! symbolic link is followed and its target replaced, so the link stays a link.
//!
//! Every function here blocks on the file system.

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// Links followed before giving up (Linux's own limit).
const MAX_LINKS: usize = 40;

/// The mode a file that did not exist is created with (Unix; nothing on Windows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewFileMode {
    /// What the umask gives (usually 0644): an ordinary document.
    Default,
    /// 0600: a file that may hold someone's tokens or keys.
    OwnerOnly,
}

/// The file a symbolic link points to (following links to links), or `path` itself. A link
/// to a file that does not exist yet gives that file's path, so writing creates it and the
/// link keeps working.
pub fn follow_link(path: &Path) -> io::Result<PathBuf> {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_LINKS {
        match fs::symlink_metadata(&current) {
            Ok(meta) if meta.file_type().is_symlink() => {
                let to = fs::read_link(&current)?;
                current = match current.parent() {
                    Some(parent) if to.is_relative() => parent.join(to),
                    _ => to,
                };
            }
            _ => return Ok(current),
        }
    }
    Err(io::Error::other(format!(
        "{} goes through too many symbolic links",
        path.display()
    )))
}

/// Replaces `path` (or, if it is a symbolic link, the file it points to) with `bytes`: a
/// temporary file in the same folder, flushed and renamed over it. On Unix it keeps the old
/// file's mode; a file that did not exist gets `new_file`'s.
pub fn replace(path: &Path, bytes: &[u8], new_file: NewFileMode) -> io::Result<()> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let target = follow_link(path)?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("the path has no file name"))?;
    let dir = folder_of(&target);
    let temporary = dir.join(format!(
        ".{}.{}-{}.spelaion-tmp",
        name.to_string_lossy(),
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = (|| -> io::Result<()> {
        let existing = fs::metadata(&target).ok();
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Owner-only while the bytes go in, whatever the umask; the old file's mode is
            // given only once it is complete.
            options.mode(match (&existing, new_file) {
                (Some(_), _) | (None, NewFileMode::OwnerOnly) => 0o600,
                (None, NewFileMode::Default) => 0o666,
            });
        }
        #[cfg(not(unix))]
        let _ = new_file;
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        // Not on Windows, where the only permission is "read-only" and the rename must fail
        // on such a file.
        #[cfg(unix)]
        if let Some(meta) = &existing {
            fs::set_permissions(&temporary, meta.permissions())?;
        }
        fs::rename(&temporary, &target)
    })();
    if let Err(e) = written {
        let _ = fs::remove_file(&temporary);
        return Err(e);
    }
    sync_folder(&dir);
    Ok(())
}

/// Writes `bytes` to `path`, which must not exist yet, with `like`'s mode on Unix (a backup of
/// `like`): owner-only while it is written, then that mode.
pub fn create_like(path: &Path, bytes: &[u8], like: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    let written = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        fs::set_permissions(path, fs::metadata(like)?.permissions())?;
        #[cfg(not(unix))]
        let _ = like;
        Ok(())
    })();
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(path);
        return Err(e);
    }
    sync_folder(&folder_of(path));
    Ok(())
}

fn folder_of(path: &Path) -> PathBuf {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// A rename or a new file is durable once its folder is (Unix; Windows has no such call).
fn sync_folder(dir: &Path) {
    #[cfg(unix)]
    if let Ok(folder) = fs::File::open(dir) {
        let _ = folder.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("spelaion-atomic-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    #[test]
    fn replacing_leaves_only_the_new_file() {
        let dir = folder("replace");
        let path = dir.join("a.txt");
        replace(&path, b"one", NewFileMode::Default).unwrap();
        replace(&path, b"two", NewFileMode::Default).unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"two");
        assert_eq!(names(&dir), ["a.txt"]);
    }

    #[test]
    fn a_backup_never_replaces_a_file() {
        let dir = folder("create");
        let original = dir.join("a.txt");
        fs::write(&original, "old").unwrap();
        let backup = dir.join("a.txt.bak");
        create_like(&backup, b"old", &original).unwrap();
        assert_eq!(fs::read(&backup).unwrap(), b"old");
        assert_eq!(
            create_like(&backup, b"new", &original).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&backup).unwrap(), b"old");
    }

    #[cfg(unix)]
    #[test]
    fn modes_are_kept_and_new_files_follow_the_choice() {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let dir = folder("modes");
        for kept in [0o600, 0o640, 0o755] {
            let path = dir.join(format!("{kept:o}"));
            fs::write(&path, "old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(kept)).unwrap();
            replace(&path, b"new", NewFileMode::OwnerOnly).unwrap();
            assert_eq!(mode(&path), kept);
            let backup = dir.join(format!("{kept:o}.bak"));
            create_like(&backup, b"old", &path).unwrap();
            assert_eq!(mode(&backup), kept);
        }
        let private = dir.join("private");
        replace(&private, b"x", NewFileMode::OwnerOnly).unwrap();
        assert_eq!(mode(&private), 0o600);
        // The umask decides an ordinary new file; it never grants more than 0666.
        let ordinary = dir.join("ordinary");
        replace(&ordinary, b"x", NewFileMode::Default).unwrap();
        assert_eq!(mode(&ordinary) & !0o666, 0);
        assert_ne!(mode(&ordinary) & 0o600, 0);
    }

    #[cfg(unix)]
    #[test]
    fn links_are_followed_relative_chained_and_dangling() {
        use std::os::unix::fs::symlink;
        let dir = folder("links");
        fs::create_dir(dir.join("dotfiles")).unwrap();
        let real = dir.join("dotfiles").join("real");
        fs::write(&real, "old").unwrap();
        // first -> second (relative) -> dotfiles/real
        symlink("dotfiles/real", dir.join("second")).unwrap();
        symlink(dir.join("second"), dir.join("first")).unwrap();
        replace(&dir.join("first"), b"new", NewFileMode::Default).unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"new");
        for link in ["first", "second"] {
            assert!(
                fs::symlink_metadata(dir.join(link))
                    .unwrap()
                    .file_type()
                    .is_symlink()
            );
        }
        assert_eq!(names(&dir.join("dotfiles")), ["real"]);
        // A link to a file that is not there yet creates that file.
        symlink("dotfiles/later", dir.join("dangling")).unwrap();
        replace(&dir.join("dangling"), b"x", NewFileMode::OwnerOnly).unwrap();
        assert_eq!(fs::read(dir.join("dotfiles").join("later")).unwrap(), b"x");
        // A loop is an error, not a hang.
        symlink(dir.join("loop-b"), dir.join("loop-a")).unwrap();
        symlink(dir.join("loop-a"), dir.join("loop-b")).unwrap();
        assert!(replace(&dir.join("loop-a"), b"x", NewFileMode::Default).is_err());
    }
}
