//! DuckDB as an engine plugin (ADR-041): this crate's driver, served over the plugin protocol.

use std::sync::Arc;

use spelaion_driver_duckdb::DuckdbDriver;
use spelaion_plugin_sdk::{Plugin, run};

#[tokio::main]
async fn main() {
    run(Plugin::new(
        "spelaiondb-plugin-duckdb",
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(DuckdbDriver::default())],
    ))
    .await
}
