//! The plugin's side of the plugin protocol (ADR-041, `docs/plugin-protocol.md`).
//!
//! A plugin written in Rust implements `spelaion-driver-api`'s [`Driver`] and `Connection`, as
//! a driver compiled into the core does, and serves them:
//!
//! ```no_run
//! # use std::sync::Arc;
//! # fn driver() -> Arc<dyn spelaion_driver_api::Driver> { unimplemented!() }
//! #[tokio::main]
//! async fn main() {
//!     spelaion_plugin_sdk::run(spelaion_plugin_sdk::Plugin::new(
//!         env!("CARGO_PKG_NAME"),
//!         env!("CARGO_PKG_VERSION"),
//!         vec![driver()],
//!     ))
//!     .await
//! }
//! ```
//!
//! **Standard output is the protocol.** A plugin never prints to it (`println!`); what it has
//! to say goes to standard error, which the core logs. It never reads a password from anywhere
//! but the `connect` it is sent, and it runs what it is sent: whether a statement may run was
//! decided by the core.

mod serve;

use std::sync::Arc;

use spelaion_driver_api::Driver;
pub use spelaion_plugin_protocol::{DEFAULT_CREDIT, VERSION};

pub use serve::serve;

/// What a plugin is: its name and version, and the drivers it serves (one per engine name).
pub struct Plugin {
    pub name: String,
    pub version: String,
    pub drivers: Vec<Arc<dyn Driver>>,
    /// How many record batches may be sent on a stream without credit given back.
    pub credit: u32,
}

impl Plugin {
    pub fn new(name: &str, version: &str, drivers: Vec<Arc<dyn Driver>>) -> Self {
        Self {
            name: name.into(),
            version: version.into(),
            drivers,
            credit: DEFAULT_CREDIT,
        }
    }
}

/// Serves `plugin` on the program's standard input and output until standard input closes,
/// then ends the program: a plugin never outlives the program that started it, whatever its
/// drivers still have running.
pub async fn run(plugin: Plugin) -> ! {
    let code = match serve(plugin, tokio::io::stdin(), tokio::io::stdout()).await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("the plugin protocol failed: {error}");
            1
        }
    };
    std::process::exit(code)
}
