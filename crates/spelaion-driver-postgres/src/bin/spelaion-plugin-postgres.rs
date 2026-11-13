//! PostgreSQL as an engine plugin (ADR-041): this crate's driver, served over the plugin protocol.

use std::sync::Arc;

use spelaion_driver_postgres::PostgresDriver;
use spelaion_plugin_sdk::{Plugin, run};

#[tokio::main]
async fn main() {
    run(Plugin::new(
        "spelaiondb-plugin-postgres",
        env!("CARGO_PKG_VERSION"),
        vec![Arc::new(PostgresDriver)],
    ))
    .await
}
