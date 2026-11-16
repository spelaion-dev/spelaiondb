//! `spelaion-write-catalogue`, the program: what it is told, and what it leaves. What the next
//! catalogue may hold is the core's (`Catalogue::next`) and is tested there.

use std::path::PathBuf;
use std::process::{Command, Output};

use spelaion_core::plugins::catalogue::CatalogueKey;
use spelaion_core::plugins::pins::Pins;

const TOOL: &str = env!("CARGO_BIN_EXE_spelaion-write-catalogue");

/// A folder of the test, removed when it ends.
struct Scratch(PathBuf);

impl Scratch {
    fn new(label: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "spelaion-write-catalogue-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(TOOL).args(args).output().expect("the tool")
}

#[test]
fn the_first_catalogue_has_serial_one_and_the_pins() {
    let scratch = Scratch::new("first");
    let out = scratch.0.join("plugins-catalogue.toml");
    let ran = run(&["--first", "--out", out.to_str().unwrap()]);
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stderr)
    );
    let text = std::fs::read_to_string(&out).expect("the catalogue's text");
    assert!(text.contains("schema_version = 1\nserial = 1\n"), "{text}");
    // Every pin of this checkout's plugins.toml is in it.
    for plugin in Pins::shipped().plugins() {
        assert!(
            text.contains(&format!("name = \"{}\"", plugin.name)),
            "{text}"
        );
        assert!(
            text.contains(&format!("version = \"{}\"", plugin.version)),
            "{text}"
        );
    }
    assert_eq!(
        text.matches("[[plugin]]").count(),
        Pins::shipped().plugins().len()
    );
    assert_eq!(
        text.matches("[[withdrawn]]").count(),
        Pins::shipped().withdrawn().len()
    );
    // It says nothing on standard output, and no secret is asked for.
    assert!(ran.stdout.is_empty());
}

/// The tool never guesses that there is no published catalogue: a failed download must not
/// start the serial again, which every app would refuse.
#[test]
fn it_must_be_told_which_catalogue_is_published() {
    let scratch = Scratch::new("told");
    let out = scratch.0.join("plugins-catalogue.toml");
    let out_arg = out.to_str().unwrap();
    let absent = scratch.0.join("absent.signed");
    let odd = scratch.0.join("odd.signed");
    std::fs::write(
        &odd,
        b"untrusted comment: x\nAAAA\ntrusted comment: y\nBBBB\nserial = 1\n",
    )
    .unwrap();

    let attempts: Vec<(&str, Vec<&str>)> = vec![
        ("neither", vec!["--out", out_arg]),
        (
            "both",
            vec![
                "--first",
                "--published",
                odd.to_str().unwrap(),
                "--out",
                out_arg,
            ],
        ),
        (
            "a published file that is not there",
            vec!["--published", absent.to_str().unwrap(), "--out", out_arg],
        ),
        (
            "a published file we did not sign",
            vec!["--published", odd.to_str().unwrap(), "--out", out_arg],
        ),
        ("no place to write", vec!["--first"]),
        (
            "something else",
            vec!["--first", "--out", out_arg, "--force"],
        ),
    ];
    for (what, args) in attempts {
        // A catalogue from an earlier run is not this run's answer.
        std::fs::write(&out, b"left from before").unwrap();
        let ran = run(&args);
        assert!(!ran.status.success(), "told {what}, it went on");
        if args.contains(&"--out") && !args.contains(&"--force") {
            assert!(!out.exists(), "told {what}, it left a catalogue behind");
        }
    }
    // Why a file we did not sign is refused depends on whether this build has a key yet.
    let ran = run(&["--published", odd.to_str().unwrap(), "--out", out_arg]);
    let said = String::from_utf8_lossy(&ran.stderr);
    match CatalogueKey::shipped() {
        None => assert!(said.contains("no catalogue key"), "{said}"),
        Some(_) => assert!(said.contains("is not the published catalogue"), "{said}"),
    }
}
