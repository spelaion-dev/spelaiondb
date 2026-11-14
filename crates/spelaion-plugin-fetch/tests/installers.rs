//! The installers carry every pinned plugin (ADR-042, ADR-057): the release fetches each pinned
//! build's archive for the installer's platform into `apps/desktop/src-tauri/plugins`, and
//! `tauri.release.conf.json` puts every archive there into the installer's resources, where the
//! app and the `spelaiondb` next to it look for them (`spelaion_core::plugins::bundled_folder`).
//! The programs themselves are not carried: a signed app (macOS) and an AppImage change them.

use spelaion_core::plugins::pins::Pins;

const RELEASE_CONFIG: &str =
    include_str!("../../../apps/desktop/src-tauri/tauri.release.conf.json");
const APP_CONFIG: &str = include_str!("../../../apps/desktop/src-tauri/tauri.conf.json");

/// The programs the release config ships next to the app, by name.
fn shipped_programs() -> Vec<String> {
    // The file is small and flat: the names after `binaries/`, without a JSON dependency.
    RELEASE_CONFIG
        .split("\"binaries/")
        .skip(1)
        .map(|rest| rest.split('"').next().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn the_installers_carry_the_cli_and_every_pinned_archive() {
    // Only the CLI is a program next to the app.
    assert_eq!(
        shipped_programs(),
        ["spelaiondb"],
        "tauri.release.conf.json's externalBin"
    );
    // Every archive the release fetched, into the folder the core looks in.
    assert!(
        RELEASE_CONFIG.contains(r#""plugins/*.tar.gz": "plugins/""#),
        "tauri.release.conf.json must put the fetched archives in the resources' plugins folder"
    );
    // Every archive is a `.tar.gz`, which is what the resource pattern picks up.
    for plugin in Pins::shipped().plugins() {
        for (platform, build) in &plugin.platforms {
            assert!(
                build.archive.ends_with(".tar.gz"),
                "{} {} for {platform}: {} is not a .tar.gz",
                plugin.name,
                plugin.version,
                build.archive
            );
        }
    }
    // A pinned plugin has a build for every platform an installer is made for: one without it
    // would leave that platform's installers without its engines.
    for plugin in Pins::shipped().plugins() {
        for platform in spelaion_core::plugins::pins::PLATFORMS {
            assert!(
                plugin.platforms.contains_key(platform),
                "{} is pinned without a build for {platform}",
                plugin.name
            );
        }
    }
}

/// On Linux the resources' folder is named after the product: the core's name for it must be
/// the app's.
#[test]
fn the_core_names_the_resources_folder_as_the_app_does() {
    let product = APP_CONFIG
        .split("\"productName\"")
        .nth(1)
        .and_then(|rest| rest.split('"').nth(1))
        .expect("tauri.conf.json has a productName");
    assert_eq!(product, spelaion_core::plugins::PRODUCT_NAME);
}

/// Where the installers put the resources, from the folder the app and the CLI are in.
#[test]
fn the_archives_are_looked_for_where_each_installer_puts_its_resources() {
    use std::path::Path;
    let program_folder = if cfg!(windows) {
        Path::new(r"C:\Program Files\SpelaionDB")
    } else if cfg!(target_os = "macos") {
        Path::new("/Applications/SpelaionDB.app/Contents/MacOS")
    } else {
        Path::new("/usr/bin")
    };
    let expected = if cfg!(windows) {
        Path::new(r"C:\Program Files\SpelaionDB\plugins").to_path_buf()
    } else if cfg!(target_os = "macos") {
        Path::new("/Applications/SpelaionDB.app/Contents/Resources/plugins").to_path_buf()
    } else {
        Path::new("/usr/lib/SpelaionDB/plugins").to_path_buf()
    };
    assert_eq!(
        spelaion_core::plugins::bundled_folder_of(program_folder),
        Some(expected)
    );
}
