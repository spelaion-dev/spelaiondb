//! SQLite as an engine plugin (ADR-041): this crate's driver, served over the plugin protocol.

use std::sync::Arc;

use spelaion_driver_sqlite::SqliteDriver;
use spelaion_plugin_sdk::{Plugin, run};

#[tokio::main]
async fn main() {
    run(Plugin::new(
        "spelaiondb-plugin-sqlite",
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(SqliteDriver::default())],
    ))
    .await
}
