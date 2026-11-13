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
use spelaion_core::plugins::pins::{Pins, platform};
use spelaion_plugin_fetch::{GITHUB, fetch};

fn main() -> anyhow::Result<()> {
    let mut into: Option<PathBuf> = None;
    let mut base = GITHUB.to_string();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--into" => into = Some(args.next().context("--into needs a folder")?.into()),
            // For a mirror, or the tests: something laid out like GitHub's releases.
            "--base" => base = args.next().context("--base needs an address")?,
            "--help" | "-h" => {
                eprintln!("usage: spelaion-fetch-plugins --into <folder> [--base <address>]");
                return Ok(());
            }
            other => bail!("unknown argument {other:?} (see --help)"),
        }
    }
    let into = into.context("--into <folder> says where the programs go")?;
    let Some(platform) = platform() else {
        bail!("no plugin is built for this platform");
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
