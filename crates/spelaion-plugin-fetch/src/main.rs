//! `spelaion-fetch-plugins`: fetches the plugin builds `plugins.toml` pins for this platform,
//! checks them and prints the variables that point a development build at them.
//!
//!     cargo run -p spelaion-plugin-fetch -- --into target/plugins
//!     cargo run -p spelaion-plugin-fetch -- --into target/plugins >> "$GITHUB_ENV"
//!
//! The pins are the ones compiled in: this repository's `plugins.toml` as it is checked out.
//! What it prints on standard output is only `NAME=value` lines; everything else goes to
//! standard error.

use std::path::PathBuf;

use anyhow::{Context, bail};
use spelaion_core::plugins::pins::{PLATFORMS, Pins, platform};
use spelaion_plugin_fetch::{GITHUB, fetch};

fn main() -> anyhow::Result<()> {
    let mut into: Option<PathBuf> = None;
    let mut base = GITHUB.to_string();
    let mut target: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--into" => into = Some(args.next().context("--into needs a folder")?.into()),
            // For a mirror, or the tests: something laid out like GitHub's releases.
            "--base" => base = args.next().context("--base needs an address")?,
            // The release builds an installer for a platform that may not be the runner's (an
            // Intel Mac's, on an Apple silicon runner): the plugins it carries are that one's.
            "--platform" => target = Some(args.next().context("--platform needs a target")?),
            "--help" | "-h" => {
                eprintln!(
                    "usage: spelaion-fetch-plugins --into <folder> [--platform <target>] \
                     [--base <address>]"
                );
                return Ok(());
            }
            other => bail!("unknown argument {other:?} (see --help)"),
        }
    }
    let into = into.context("--into <folder> says where the programs go")?;
    let platform = match &target {
        Some(target) => PLATFORMS
            .into_iter()
            .find(|known| known == target)
            .with_context(|| format!("no plugin is built for {target}"))?,
        None => match platform() {
            Some(platform) => platform,
            None => bail!("no plugin is built for this platform"),
        },
    };

    let pins = Pins::shipped();
    let fetched = fetch(pins, platform, &base, &into)?;
    if fetched.is_empty() {
        eprintln!("plugins.toml pins nothing for {platform}: nothing to fetch");
    }
    for plugin in &fetched {
        eprintln!(
            "{} {}: {}",
            plugin.plugin,
            plugin.version,
            plugin.program.display()
        );
        for (name, value) in plugin.variables() {
            println!("{name}={value}");
        }
    }
    Ok(())
}
