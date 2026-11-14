//! `spelaion-write-catalogue`: writes the text of the next plugin catalogue (ADR-055), for the
//! workflow that signs and publishes it when the pins change on `dev`.
//!
//!     cargo run -p spelaion-plugin-fetch --bin spelaion-write-catalogue -- \
//!         --published plugins-catalogue.signed --out plugins-catalogue.toml
//!     cargo run -p spelaion-plugin-fetch --bin spelaion-write-catalogue -- \
//!         --first --out plugins-catalogue.toml
//!
//! The pins are the ones compiled in: this repository's `plugins.toml` as it is checked out.
//! The published catalogue is its one signed file, and it is read only if the key compiled
//! into this build signed it: the next catalogue is never built on a file that is not ours.
//!
//! **It only adds** (`Catalogue::next`): what was published stays, a published version never
//! changes, and a withdrawn build is never pinned again. When the pins say nothing the
//! published catalogue does not already say, nothing is written and the serial is not spent:
//! the workflow publishes only if the file exists.
//!
//! It signs nothing and reads no secret: signing is the workflow's next step.

use std::path::PathBuf;

use anyhow::{Context, bail};
use spelaion_core::plugins::catalogue::{Catalogue, CatalogueKey};
use spelaion_core::plugins::pins::Pins;

const USAGE: &str =
    "usage: spelaion-write-catalogue (--published <its signed file> | --first) --out <file>";

fn main() -> anyhow::Result<()> {
    let mut published: Option<PathBuf> = None;
    let mut first = false;
    let mut out: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--published" => {
                published = Some(args.next().context("--published needs a file")?.into());
            }
            "--first" => first = true,
            "--out" => out = Some(args.next().context("--out needs a file")?.into()),
            "--help" | "-h" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    let out = out.context("--out <file> says where the next catalogue's text goes")?;
    // Whatever was there is not this run's answer.
    match std::fs::remove_file(&out) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).with_context(|| format!("removing {}", out.display())),
    }

    let published = match (published, first) {
        (Some(file), false) => {
            let key = CatalogueKey::shipped().context(
                "this build has no catalogue key, so it cannot tell whether the published \
                 catalogue is ours",
            )?;
            let signed =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let catalogue = Catalogue::verify_signed(&key, &signed)
                .with_context(|| format!("{} is not the published catalogue", file.display()))?;
            Some(catalogue)
        }
        // Said in so many words: a download that failed must not start the serial again.
        (None, true) => None,
        _ => bail!(
            "say which catalogue is published now (--published), or --first if none ever \
             was\n{USAGE}"
        ),
    };

    let pins = Pins::shipped();
    match Catalogue::next(published.as_ref(), pins)? {
        None => eprintln!(
            "the published catalogue already says what plugins.toml says: nothing to publish"
        ),
        Some(text) => {
            std::fs::write(&out, &text).with_context(|| format!("writing {}", out.display()))?;
            eprintln!(
                "{}: the catalogue after {}, from {} pinned and {} withdrawn in plugins.toml",
                out.display(),
                match &published {
                    Some(published) => format!("serial {}", published.serial()),
                    None => "none".to_string(),
                },
                pins.plugins().len(),
                pins.withdrawn().len(),
            );
        }
    }
    Ok(())
}
