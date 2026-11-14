//! The installers carry every pinned plugin (ADR-042): what the release puts next to the app
//! is the list in `tauri.release.conf.json`, and it must be the pins. An engine that is pinned
//! and not in that list would be missing from every installer.

use spelaion_core::plugins::pins::Pins;

const RELEASE_CONFIG: &str =
    include_str!("../../../apps/desktop/src-tauri/tauri.release.conf.json");

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
fn the_installers_carry_every_pinned_plugin_and_nothing_else() {
    let shipped = shipped_programs();
    assert!(
        shipped.contains(&"spelaiondb".to_string()),
        "the CLI ships next to the app: {shipped:?}"
    );
    let mut plugins: Vec<&str> = shipped
        .iter()
        .map(String::as_str)
        .filter(|name| name.starts_with("spelaiondb-plugin-"))
        .collect();
    plugins.sort_unstable();
    let mut pinned: Vec<&str> = Pins::shipped()
        .plugins()
        .iter()
        .map(|plugin| plugin.name.as_str())
        .collect();
    pinned.sort_unstable();
    assert_eq!(
        plugins, pinned,
        "tauri.release.conf.json's externalBin and plugins.toml name different plugins"
    );
    // A pinned plugin has a build for every platform an installer is made for: one without it
    // would break that platform's release.
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
