//! A check every export that replaces a file passes (Unix only: Windows has neither modes nor,
//! for an ordinary user, symbolic links).

use std::fs;
use std::os::unix::fs::{PermissionsExt as _, symlink};
use std::path::Path;

/// `export` writes something to the path it is given (never the text `old`). Exporting over
/// a 0600 file keeps 0600, exporting to a symbolic link writes the file it points to and the
/// link stays a link, and no temporary file is left behind.
pub(crate) fn assert_keeps_mode_and_links(label: &str, export: impl Fn(&Path)) {
    let dir = std::env::temp_dir().join(format!(
        "spelaion-export-files-{label}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("real")).unwrap();
    let private = dir.join("private.out");
    fs::write(&private, "old").unwrap();
    fs::set_permissions(&private, fs::Permissions::from_mode(0o600)).unwrap();
    export(&private);
    assert_ne!(fs::read(&private).unwrap(), b"old", "{label}: not written");
    assert_eq!(
        fs::metadata(&private).unwrap().permissions().mode() & 0o7777,
        0o600,
        "{label}: the file's mode was not kept"
    );
    let real = dir.join("real").join("target.out");
    fs::write(&real, "old").unwrap();
    let link = dir.join("link.out");
    symlink("real/target.out", &link).unwrap();
    export(&link);
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "{label}: the link was replaced by a file"
    );
    assert_ne!(
        fs::read(&real).unwrap(),
        b"old",
        "{label}: the link's target was not written"
    );
    let names = |dir: &Path| {
        let mut names: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    };
    assert_eq!(names(&dir), ["link.out", "private.out", "real"], "{label}");
    assert_eq!(names(&dir.join("real")), ["target.out"], "{label}");
    fs::remove_dir_all(&dir).unwrap();
}
