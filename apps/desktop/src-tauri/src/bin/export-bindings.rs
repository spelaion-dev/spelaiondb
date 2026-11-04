//! Generates `apps/desktop/src/bindings.ts` with the types of the Tauri commands.
//! `cargo run -p spelaion-desktop --features bindings --bin export-bindings`. It is a binary and
//! not a test because on Windows the tests of a Tauri crate do not start (they lack the app
//! manifest); the feature keeps it out of the app's bundle.

fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/bindings.ts");
    spelaion_desktop_lib::export_bindings(&path).expect("exporting bindings");
    println!("{}", path.display());
}
