//! Fetching pinned plugins, against a folder laid out like GitHub's releases: nothing that is
//! not the pinned archive, holding the pinned program, ever reaches the folder of programs.
//! Each test tries one way around it.
//!
//! The "program" here is a file of text: these tests fetch and check it, they never start it.

use std::path::{Path, PathBuf};
use std::process::Command;

use spelaion_core::plugins::pins::{Pins, platform, program_file_name, sha256_of};
use spelaion_plugin_fetch::{FetchError, fetch, fetch_keeping_archives};

const NAME: &str = "spelaiondb-plugin-test";
const VERSION: &str = "0.1.0";

/// A folder of the test, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("spelaion-fetch-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    /// The address `fetch` is given: this folder, as a `file:` URL.
    fn base(&self) -> String {
        let path = self
            .0
            .join("releases")
            .display()
            .to_string()
            .replace('\\', "/");
        format!("file:///{}", path.trim_start_matches('/'))
    }

    /// Where the programs go.
    fn programs(&self) -> PathBuf {
        self.0.join("plugins")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn here() -> &'static str {
    platform().expect("the tests run on a platform plugins are built for")
}

/// The system's `tar`, as the tool finds it.
fn tar() -> PathBuf {
    if cfg!(windows)
        && let Some(root) = std::env::var_os("SystemRoot")
    {
        return Path::new(&root).join("System32").join("tar.exe");
    }
    PathBuf::from("tar")
}

/// The archive's name on this platform, as a plugin's release names it.
fn archive_name() -> String {
    let extension = if cfg!(windows) { "zip" } else { "tar.gz" };
    format!("{NAME}-{VERSION}-{}.{extension}", here())
}

/// A release in `scratch`: an archive that holds `members` (paths inside it, with their
/// contents). Returns the archive's path.
fn release(scratch: &Scratch, members: &[(&str, &[u8])]) -> PathBuf {
    release_named(scratch, &archive_name(), members)
}

/// The same, with the archive named `archive` (its extension says its format).
fn release_named(scratch: &Scratch, archive: &str, members: &[(&str, &[u8])]) -> PathBuf {
    let pack = scratch.0.join("pack");
    let _ = std::fs::remove_dir_all(&pack);
    for (member, contents) in members {
        let file = pack.join(member);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, contents).unwrap();
    }
    let folder = scratch
        .0
        .join("releases")
        .join("spelaion-dev")
        .join(NAME)
        .join("releases")
        .join("download")
        .join(format!("v{VERSION}"));
    std::fs::create_dir_all(&folder).unwrap();
    let archive = folder.join(archive);
    let _ = std::fs::remove_file(&archive);
    let mut command = Command::new(tar());
    // `-a`: the format the name says (zip on Windows, gzip elsewhere).
    command
        .arg("-a")
        .arg("-cf")
        .arg(&archive)
        .arg("-C")
        .arg(&pack);
    for (member, _) in members {
        command.arg(member);
    }
    let made = command.output().expect("tar runs");
    assert!(
        made.status.success(),
        "{}",
        String::from_utf8_lossy(&made.stderr)
    );
    archive
}

/// The pins of that release, with whatever hashes and size the test gives.
fn pins(archive_sha256: &str, archive_size: u64, program_sha256: &str) -> Pins {
    pins_for(
        here(),
        &archive_name(),
        archive_sha256,
        archive_size,
        program_sha256,
    )
}

/// The same, for `platform`'s build in `archive`.
fn pins_for(
    platform: &str,
    archive: &str,
    archive_sha256: &str,
    archive_size: u64,
    program_sha256: &str,
) -> Pins {
    Pins::parse(&format!(
        r#"schema_version = 1

[[plugin]]
name = "{NAME}"
repository = "spelaion-dev/{NAME}"
version = "{VERSION}"
protocol = "1.0"
engines = ["test", "test2"]
min_app = "0.10.0"

[plugin.platform.{platform}]
archive = "{archive}"
archive_sha256 = "{archive_sha256}"
archive_size = {archive_size}
program_sha256 = "{program_sha256}"
"#,
    ))
    .expect("the test's pins")
}

/// The pins that are true of `archive` and of a program whose contents are `program`.
fn true_pins(scratch: &Scratch, archive: &Path, program: &[u8]) -> Pins {
    let file = scratch.0.join("the-program");
    std::fs::write(&file, program).unwrap();
    pins(
        &sha256_of(archive).unwrap(),
        std::fs::metadata(archive).unwrap().len(),
        &sha256_of(&file).unwrap(),
    )
}

/// Nothing was left where the programs go: no program, no half-done download.
fn nothing_arrived(scratch: &Scratch) {
    let left: Vec<String> = std::fs::read_dir(scratch.programs())
        .map(|entries| {
            entries
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    assert!(left.is_empty(), "left behind: {left:?}");
}

const PROGRAM: &[u8] = b"the pinned build of the program";

#[test]
fn a_pinned_release_arrives_checked() {
    let scratch = Scratch::new("good");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, PROGRAM), ("LICENSE", b"Apache-2.0")]);
    let pins = true_pins(&scratch, &archive, PROGRAM);

    let fetched = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap();
    assert_eq!(fetched.len(), 1);
    let plugin = &fetched[0];
    assert_eq!(
        (plugin.plugin.as_str(), plugin.version.as_str()),
        (NAME, VERSION)
    );
    assert_eq!(plugin.program, scratch.programs().join(&file));
    assert_eq!(std::fs::read(&plugin.program).unwrap(), PROGRAM);
    assert!(plugin.program.is_absolute(), "{:?}", plugin.program);
    // Only the program arrived: not the licence, not the archive, not the work folder.
    let arrived: Vec<_> = std::fs::read_dir(scratch.programs())
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(arrived, std::slice::from_ref(&file));
    // One variable for each engine it serves, both at this program.
    let program = plugin.program.display().to_string();
    assert_eq!(
        plugin.variables(),
        [
            ("SPELAION_DEV_PLUGIN_TEST".to_string(), program.clone()),
            ("SPELAION_DEV_PLUGIN_TEST2".to_string(), program),
        ]
    );

    // Fetched again, it replaces what was there.
    std::fs::write(&plugin.program, b"an older build").unwrap();
    fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap();
    assert_eq!(std::fs::read(&plugin.program).unwrap(), PROGRAM);
}

/// A pin for another platform fetches nothing here.
#[test]
fn only_this_platforms_builds_are_fetched() {
    let scratch = Scratch::new("platform");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, PROGRAM)]);
    let pins = true_pins(&scratch, &archive, PROGRAM);
    let other = spelaion_core::plugins::pins::PLATFORMS
        .into_iter()
        .find(|p| *p != here())
        .unwrap();
    let fetched = fetch(&pins, other, &scratch.base(), &scratch.programs()).unwrap();
    assert!(fetched.is_empty());
    nothing_arrived(&scratch);
}

/// `--platform` for another platform's installer (SDB-463): the program is named as on that
/// platform, whatever this machine is, so a Linux build and a Windows build both arrive from
/// here. Both archives are `.tar.gz`, which every host's `tar` writes and reads: the test is
/// about the program's name, not the archive's format.
#[test]
fn another_platforms_program_is_named_as_there() {
    for (target, file) in [
        ("x86_64-unknown-linux-gnu", NAME.to_string()),
        ("x86_64-pc-windows-msvc", format!("{NAME}.exe")),
    ] {
        assert_eq!(spelaion_plugin_fetch::program_file_for(NAME, target), file);
        let scratch = Scratch::new(&format!("target-{target}"));
        let archive_name = format!("{NAME}-{VERSION}-{target}.tar.gz");
        let archive = release_named(&scratch, &archive_name, &[(&file, PROGRAM)]);
        let program = scratch.0.join("the-program");
        std::fs::write(&program, PROGRAM).unwrap();
        let pins = pins_for(
            target,
            &archive_name,
            &sha256_of(&archive).unwrap(),
            std::fs::metadata(&archive).unwrap().len(),
            &sha256_of(&program).unwrap(),
        );

        let fetched = fetch(&pins, target, &scratch.base(), &scratch.programs())
            .unwrap_or_else(|e| panic!("{target}: {e}"));
        assert_eq!(fetched.len(), 1, "{target}");
        assert_eq!(
            fetched[0].program,
            scratch.programs().join(&file),
            "{target}"
        );
        assert_eq!(std::fs::read(&fetched[0].program).unwrap(), PROGRAM);
    }
}

/// The release was replaced after it was pinned: the same size, other bytes.
#[test]
fn an_archive_that_is_not_the_pinned_one_is_refused_before_it_is_unpacked() {
    let scratch = Scratch::new("swapped");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, PROGRAM)]);
    let pinned = sha256_of(&archive).unwrap();
    // Another release under the same name. The pin's size is this one's, so that only the
    // hash tells them apart.
    let evil: Vec<u8> = PROGRAM.iter().map(|b| b.to_ascii_uppercase()).collect();
    let swapped = release(&scratch, &[(&file, &evil)]);
    assert_ne!(sha256_of(&swapped).unwrap(), pinned);
    let size = std::fs::metadata(&swapped).unwrap().len();
    let pins = pins(&pinned, size, &pinned);

    let error = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap_err();
    assert!(matches!(error, FetchError::Hash { .. }), "{error}");
    assert!(
        error.to_string().contains(&format!("{NAME} {VERSION}")),
        "{error}"
    );
    nothing_arrived(&scratch);
}

#[test]
fn an_archive_of_another_size_is_refused() {
    let scratch = Scratch::new("size");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, PROGRAM)]);
    let hash = sha256_of(&archive).unwrap();
    let size = std::fs::metadata(&archive).unwrap().len();
    // The pin says it is bigger, and smaller: neither is this archive.
    for pinned in [size + 1, size - 1] {
        let pins = pins(&hash, pinned, &hash);
        let error = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap_err();
        assert!(
            matches!(error, FetchError::Size { .. } | FetchError::Download { .. }),
            "{error}"
        );
        nothing_arrived(&scratch);
    }
}

/// The archive is the pinned one, and the program inside is not the pinned program.
#[test]
fn a_program_that_is_not_the_pinned_one_never_reaches_the_folder() {
    let scratch = Scratch::new("program");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, b"another program")]);
    // True of the archive, and of the program the pin was meant for.
    let pins = true_pins(&scratch, &archive, PROGRAM);

    let error = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap_err();
    assert!(matches!(error, FetchError::Program { .. }), "{error}");
    assert!(
        error
            .to_string()
            .contains("is not the build that was pinned"),
        "{error}"
    );
    nothing_arrived(&scratch);
}

/// The program must be at the archive's top, by its own name.
#[test]
fn an_archive_that_is_not_a_plugins_release_is_refused() {
    let file = program_file_name(NAME);
    let in_a_folder = format!("bin/{file}");
    for (label, members) in [
        ("folder", vec![(in_a_folder.as_str(), PROGRAM)]),
        ("other-name", vec![("spelaiondb-plugin-other", PROGRAM)]),
        // The program is there, and so is something in a folder: a release holds neither
        // folders nor anything that could land outside where it is unpacked.
        (
            "extra-folder",
            vec![
                (file.as_str(), PROGRAM),
                ("extra/readme.txt", b"hello".as_slice()),
            ],
        ),
    ] {
        let scratch = Scratch::new(label);
        let archive = release(&scratch, &members);
        let pins = true_pins(&scratch, &archive, PROGRAM);
        let error = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap_err();
        assert!(
            matches!(error, FetchError::Archive { .. }),
            "{label}: {error}"
        );
        nothing_arrived(&scratch);
    }
}

#[test]
fn a_release_that_is_not_there_fails_naming_it() {
    let scratch = Scratch::new("missing");
    let hash = "0".repeat(64);
    let pins = pins(&hash, 1, &hash);
    let error = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap_err();
    assert!(matches!(error, FetchError::Download { .. }), "{error}");
    let text = error.to_string();
    assert!(text.contains(&format!("{NAME} {VERSION}")), "{text}");
    assert!(text.contains(&archive_name()), "{text}");
    nothing_arrived(&scratch);
}

/// With nothing pinned there is nothing to fetch, and that is not an error.
#[test]
fn no_pins_fetch_nothing() {
    let scratch = Scratch::new("none");
    let fetched = fetch(&Pins::none(), here(), &scratch.base(), &scratch.programs()).unwrap();
    assert!(fetched.is_empty());
}

/// The release keeps the archives for the installers (ADR-057): only one that passed every
/// check, as the release has it.
#[test]
fn a_kept_archive_is_the_checked_release() {
    let scratch = Scratch::new("kept");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, PROGRAM), ("LICENSE", b"Apache-2.0")]);
    let pins = true_pins(&scratch, &archive, PROGRAM);
    let kept_in = scratch.0.join("archives");

    let fetched = fetch_keeping_archives(
        &pins,
        here(),
        &scratch.base(),
        &scratch.programs(),
        &kept_in,
    )
    .unwrap();
    let kept = fetched[0].archive.clone().expect("the archive was kept");
    assert_eq!(kept, kept_in.join(archive.file_name().unwrap()));
    assert_eq!(
        std::fs::read(&kept).unwrap(),
        std::fs::read(&archive).unwrap()
    );
    assert_eq!(std::fs::read(&fetched[0].program).unwrap(), PROGRAM);
    // `fetch` keeps none.
    let plain = fetch(&pins, here(), &scratch.base(), &scratch.programs()).unwrap();
    assert_eq!(plain[0].archive, None);
}

/// An archive whose program is not the pinned one is not kept either.
#[test]
fn an_archive_that_fails_a_check_is_not_kept() {
    let scratch = Scratch::new("not-kept");
    let file = program_file_name(NAME);
    let archive = release(&scratch, &[(&file, b"another program")]);
    let pins = true_pins(&scratch, &archive, PROGRAM);
    let kept_in = scratch.0.join("archives");

    let error = fetch_keeping_archives(
        &pins,
        here(),
        &scratch.base(),
        &scratch.programs(),
        &kept_in,
    )
    .unwrap_err();
    assert!(matches!(error, FetchError::Program { .. }), "{error}");
    nothing_arrived(&scratch);
    let left: Vec<_> = std::fs::read_dir(&kept_in)
        .map(|entries| entries.flatten().map(|e| e.file_name()).collect())
        .unwrap_or_default();
    assert!(left.is_empty(), "kept: {left:?}");
}
