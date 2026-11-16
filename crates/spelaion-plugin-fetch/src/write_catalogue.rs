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
//!
//! After signing, the workflow checks what it is about to publish:
//!
//!     cargo run -p spelaion-plugin-fetch --bin spelaion-write-catalogue -- \
//!         --check plugins-catalogue.signed --text plugins-catalogue.toml
//!
//! The signed file must be accepted with the key compiled into this build, as the app accepts
//! it (`Catalogue::verify_signed`), and hold exactly the text written. A build with no key
//! accepts nothing, so nothing is published until the key is compiled in.

use std::path::PathBuf;

use anyhow::{Context, bail};
use spelaion_core::plugins::catalogue::{Catalogue, CatalogueKey};
use spelaion_core::plugins::pins::Pins;

const USAGE: &str = "usage: spelaion-write-catalogue (--published <its signed file> | --first) \
                     --out <file>\n       spelaion-write-catalogue --check <signed file> --text \
                     <the text written>";

fn main() -> anyhow::Result<()> {
    let mut published: Option<PathBuf> = None;
    let mut first = false;
    let mut out: Option<PathBuf> = None;
    let mut check: Option<PathBuf> = None;
    let mut text: Option<PathBuf> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--published" => {
                published = Some(args.next().context("--published needs a file")?.into());
            }
            "--first" => first = true,
            "--out" => out = Some(args.next().context("--out needs a file")?.into()),
            "--check" => check = Some(args.next().context("--check needs a file")?.into()),
            "--text" => text = Some(args.next().context("--text needs a file")?.into()),
            "--help" | "-h" => {
                eprintln!("{USAGE}");
                return Ok(());
            }
            other => bail!("unknown argument {other:?}\n{USAGE}"),
        }
    }
    if let Some(signed) = check {
        if published.is_some() || first || out.is_some() {
            bail!("--check only checks a signed file: it writes nothing\n{USAGE}");
        }
        let text = text.context("--text <file> is the text the signed file must hold")?;
        return check_signed(&signed, &text);
    }
    if text.is_some() {
        bail!("--text goes with --check\n{USAGE}");
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

/// Whether `signed` is what may be published: accepted with the key compiled into this build,
/// as the app accepts it, and holding exactly the text in `text`.
fn check_signed(signed: &std::path::Path, text: &std::path::Path) -> anyhow::Result<()> {
    let key = CatalogueKey::shipped().context(
        "this build has no catalogue key (SHIPPED_KEY in crates/spelaion-core/src/plugins/\
         catalogue.rs is None), so no app would accept a catalogue: nothing may be published",
    )?;
    let signed_bytes =
        std::fs::read(signed).with_context(|| format!("reading {}", signed.display()))?;
    let expected = std::fs::read(text).with_context(|| format!("reading {}", text.display()))?;
    let catalogue = Catalogue::verify_signed(&key, &signed_bytes).with_context(|| {
        format!(
            "{} is not accepted with the key compiled into this build",
            signed.display()
        )
    })?;
    // The four lines of the signature, then the text: the text must be the one written.
    let text_starts = signed_bytes
        .iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'\n')
        .nth(3)
        .map(|(at, _)| at + 1);
    if text_starts.map(|at| &signed_bytes[at..]) != Some(expected.as_slice()) {
        bail!(
            "{} does not hold {} byte for byte after its signature",
            signed.display(),
            text.display()
        );
    }
    eprintln!(
        "{}: accepted, serial {}, {} versions, {} withdrawn",
        signed.display(),
        catalogue.serial(),
        catalogue.versions().len(),
        catalogue.withdrawn().len(),
    );
    Ok(())
}
