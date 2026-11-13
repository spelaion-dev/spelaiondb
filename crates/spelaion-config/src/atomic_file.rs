//! Replacing a file atomically, for every file the app rewrites in place: the editor's `.sql`
//! files (ADR-036), the AI clients' MCP files (ADR-033), and the configuration it edits
//! (connections, tunnels, connection types, `user.toml` and the trust store). New files that
//! must never replace another one (a new connection or tunnel, a copy, a move, a backup) are
//! created with [`create_new`] or [`create_like`].
//!
//! The new bytes go to a temporary file in the same folder, which is flushed and renamed over
//! the old one, so a crash leaves the old file or the new one, never part of one. On Unix the
//! new file keeps the old one's mode, and it is never readable by anyone else while it is
//! written: the temporary file is created owner-only and only then given the old mode. A
//! symbolic link is followed and its target replaced, so the link stays a link. Files written
//! in pieces (the core's exports) use [`AtomicWriter`], the streaming form of [`replace`].
//!
//! Every function here blocks on the file system.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
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
    let mut file = AtomicWriter::create(path, new_file)?;
    file.write_all(bytes)?;
    file.commit()
}

/// A file written in pieces (an export) that replaces `path` only once it is complete: the
/// streaming form of [`replace`], with the same guarantees. The bytes go to a uniquely named
/// temporary file beside the file `path` names (following symbolic links), so two writers
/// to one path never share it; [`AtomicWriter::commit`] flushes it, gives it the old file's
/// mode (Unix), renames it over the old file and makes the rename durable. Dropped without a
/// commit (an error, a cancel), it deletes the temporary file and `path` is untouched.
///
/// Writes go straight to the file: wrap it in a [`io::BufWriter`] for small writes.
#[derive(Debug)]
pub struct AtomicWriter {
    target: PathBuf,
    temporary: PathBuf,
    /// `None` once committed.
    file: Option<fs::File>,
}

impl AtomicWriter {
    /// Starts replacing `path`. The temporary file is hidden (`.<name>.<pid>-<n>.spelaion-tmp`).
    /// On Unix it is owner-only while it is written if the file exists or `new_file` is
    /// [`NewFileMode::OwnerOnly`].
    pub fn create(path: &Path, new_file: NewFileMode) -> io::Result<Self> {
        Self::open(path, None, new_file)
    }

    /// [`AtomicWriter::create`], with the temporary file named `<name><suffix>` (an export's
    /// `.part`, ADR-034, which the user may see while it is written), or
    /// `<name>.<pid>-<n><suffix>` if another writer (or a crash) left that one.
    pub fn create_with_suffix(
        path: &Path,
        suffix: &str,
        new_file: NewFileMode,
    ) -> io::Result<Self> {
        Self::open(path, Some(suffix), new_file)
    }

    fn open(path: &Path, suffix: Option<&str>, new_file: NewFileMode) -> io::Result<Self> {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        const ATTEMPTS: usize = 100;
        let target = follow_link(path)?;
        let name = target
            .file_name()
            .ok_or_else(|| io::Error::other("the path has no file name"))?
            .to_owned();
        let dir = folder_of(&target);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            // Owner-only while the bytes go in, whatever the umask; the old file's mode is
            // given only once it is complete.
            let exists = fs::metadata(&target).is_ok();
            options.mode(match (exists, new_file) {
                (true, _) | (false, NewFileMode::OwnerOnly) => 0o600,
                (false, NewFileMode::Default) => 0o666,
            });
        }
        #[cfg(not(unix))]
        let _ = new_file;
        for attempt in 0..ATTEMPTS {
            let mut temporary_name = std::ffi::OsString::new();
            let unique = || {
                format!(
                    "{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                )
            };
            match suffix {
                Some(suffix) if attempt == 0 => {
                    temporary_name.push(&name);
                    temporary_name.push(suffix);
                }
                Some(suffix) => {
                    temporary_name.push(&name);
                    temporary_name.push(format!(".{}{suffix}", unique()));
                }
                None => {
                    temporary_name.push(".");
                    temporary_name.push(&name);
                    temporary_name.push(format!(".{}.spelaion-tmp", unique()));
                }
            }
            let temporary = dir.join(temporary_name);
            match options.open(&temporary) {
                Ok(file) => {
                    return Ok(Self {
                        target,
                        temporary,
                        file: Some(file),
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free temporary file name beside {}", target.display()),
        ))
    }

    /// The file that is replaced: `path`, or the file its symbolic link points to.
    pub fn target(&self) -> &Path {
        &self.target
    }

    /// Flushes the file to disk, gives it the old file's mode (Unix; a file that did not exist
    /// keeps the mode it was created with) and renames it over the old one.
    pub fn commit(mut self) -> io::Result<()> {
        let file = self
            .file
            .take()
            .expect("the file is open until it is committed");
        let finished = (|| -> io::Result<()> {
            file.sync_all()?;
            drop(file);
            // Not on Windows, where the only permission is "read-only" and the rename must
            // fail on such a file.
            #[cfg(unix)]
            if let Ok(meta) = fs::metadata(&self.target) {
                fs::set_permissions(&self.temporary, meta.permissions())?;
            }
            fs::rename(&self.temporary, &self.target)
        })();
        if let Err(e) = finished {
            let _ = fs::remove_file(&self.temporary);
            return Err(e);
        }
        sync_folder(&folder_of(&self.target));
        Ok(())
    }

    fn file(&mut self) -> &mut fs::File {
        self.file
            .as_mut()
            .expect("the file is open until it is committed")
    }
}

impl Write for AtomicWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file().write(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.file().write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file().flush()
    }
}

impl Drop for AtomicWriter {
    fn drop(&mut self) {
        if let Some(file) = self.file.take() {
            drop(file);
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

/// Writes `bytes` to `path`, which must not exist yet, with `like`'s mode on Unix (a backup of
/// `like`, or a copy or move of it): owner-only while it is written, then that mode.
pub fn create_like(path: &Path, bytes: &[u8], like: &Path) -> io::Result<()> {
    create(path, bytes, Some(like), NewFileMode::OwnerOnly)
}

/// Writes `bytes` to `path`, which must not exist yet (it fails with
/// [`io::ErrorKind::AlreadyExists`], also on a symbolic link): a new file is never put over
/// another one, even one created a moment before. On Unix it gets `mode`'s mode (an
/// owner-only file is never readable by anyone else, not even while it is written).
pub fn create_new(path: &Path, bytes: &[u8], mode: NewFileMode) -> io::Result<()> {
    create(path, bytes, None, mode)
}

fn create(path: &Path, bytes: &[u8], like: Option<&Path>, mode: NewFileMode) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // A copy of another file is owner-only until it has that file's mode.
        options.mode(match (like, mode) {
            (Some(_), _) | (None, NewFileMode::OwnerOnly) => 0o600,
            (None, NewFileMode::Default) => 0o666,
        });
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(path)?;
    let written = (|| -> io::Result<()> {
        file.write_all(bytes)?;
        file.sync_all()?;
        #[cfg(unix)]
        if let Some(like) = like {
            fs::set_permissions(path, fs::metadata(like)?.permissions())?;
        }
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
    fn an_unfinished_writer_leaves_the_file_as_it_was() {
        let dir = folder("unfinished");
        let path = dir.join("a.txt");
        fs::write(&path, "old").unwrap();
        let mut file = AtomicWriter::create(&path, NewFileMode::Default).unwrap();
        file.write_all(b"partial").unwrap();
        drop(file);
        assert_eq!(fs::read(&path).unwrap(), b"old");
        assert_eq!(names(&dir), ["a.txt"]);
    }

    #[test]
    fn two_writers_to_one_path_never_share_a_temporary_file() {
        let dir = folder("two-writers");
        let path = dir.join("a.txt");
        for suffix in [None, Some(".part")] {
            let open = || match suffix {
                None => AtomicWriter::create(&path, NewFileMode::Default),
                Some(suffix) => {
                    AtomicWriter::create_with_suffix(&path, suffix, NewFileMode::Default)
                }
            };
            let mut first = open().unwrap();
            let mut second = open().unwrap();
            assert_ne!(first.temporary, second.temporary);
            first.write_all(b"first").unwrap();
            second.write_all(b"second").unwrap();
            first.commit().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"first");
            second.commit().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"second");
            assert_eq!(names(&dir), ["a.txt"]);
        }
        // The visible name first, then a unique one beside it.
        let first = AtomicWriter::create_with_suffix(&path, ".part", NewFileMode::Default).unwrap();
        let second =
            AtomicWriter::create_with_suffix(&path, ".part", NewFileMode::Default).unwrap();
        assert_eq!(first.temporary, dir.join("a.txt.part"));
        let second_name = second
            .temporary
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        assert!(second_name.starts_with("a.txt.") && second_name.ends_with(".part"));
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

    #[test]
    fn a_new_file_never_replaces_one() {
        let dir = folder("create-new");
        let path = dir.join("a.toml");
        create_new(&path, b"one", NewFileMode::Default).unwrap();
        assert_eq!(
            create_new(&path, b"two", NewFileMode::Default)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(fs::read(&path).unwrap(), b"one");
        assert_eq!(names(&dir), ["a.toml"]);
    }

    #[cfg(unix)]
    #[test]
    fn a_new_file_gets_the_chosen_mode_and_never_goes_through_a_link() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o7777;
        let dir = folder("create-new-modes");
        let private = dir.join("private");
        create_new(&private, b"x", NewFileMode::OwnerOnly).unwrap();
        assert_eq!(mode(&private), 0o600);
        let ordinary = dir.join("ordinary");
        create_new(&ordinary, b"x", NewFileMode::Default).unwrap();
        let probe = dir.join("probe");
        fs::write(&probe, "").unwrap();
        assert_eq!(mode(&ordinary), mode(&probe), "the umask decides");
        // Not even a dangling link: the new file must not land somewhere else.
        symlink(dir.join("elsewhere"), dir.join("link")).unwrap();
        assert_eq!(
            create_new(&dir.join("link"), b"x", NewFileMode::Default)
                .unwrap_err()
                .kind(),
            io::ErrorKind::AlreadyExists
        );
        assert!(!dir.join("elsewhere").exists());
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
