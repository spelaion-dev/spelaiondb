//! Generates `apps/desktop/src/bindings.ts` with the types of the Tauri commands.
//! `cargo run -p spelunk-desktop --bin export-bindings`. It is a binary and not a test because
//! on Windows the tests of a Tauri crate do not start (they lack the app manifest).

fn main() {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../src/bindings.ts");
    spelunk_desktop_lib::export_bindings(&path).expect("exporting bindings");
    println!("{}", path.display());
}
