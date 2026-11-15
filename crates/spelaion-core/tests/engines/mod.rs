//! Where the core's tests take an engine's driver from: the engines the core itself opens
//! (`Engines::builtin`), and so whatever stands behind each of them, a driver compiled in or
//! its plugin (ADR-041). No test names a driver crate: when an engine moves to a plugin, its
//! tests run through it unchanged.
//!
//! A development build follows `SPELAION_DEV_PLUGIN_<ENGINE>` (see `plugins.rs`), so the whole
//! suite runs through plugins built on this machine with `scripts/test-through-plugins.sh`.

// Each test file uses the engines it tests.
#![allow(dead_code)]

use std::path::Path;
use std::sync::Arc;

use spelaion_config::Engine;
use spelaion_core::Engines;
use spelaion_driver_api::Driver;

/// The driver of `engine`, as the core opens it.
pub fn driver(engine: Engine) -> Arc<dyn Driver> {
    Engines::builtin()
        .get(engine)
        .expect("every engine of the configuration has a driver")
}

pub fn postgres() -> Arc<dyn Driver> {
    driver(Engine::Postgres)
}

pub fn mysql() -> Arc<dyn Driver> {
    driver(Engine::Mysql)
}

pub fn mariadb() -> Arc<dyn Driver> {
    driver(Engine::Mariadb)
}

pub fn sqlserver() -> Arc<dyn Driver> {
    driver(Engine::Sqlserver)
}

pub fn sqlite() -> Arc<dyn Driver> {
    driver(Engine::Sqlite)
}

pub fn duckdb() -> Arc<dyn Driver> {
    driver(Engine::Duckdb)
}

/// Creates the empty database file of an embedded engine. The file must not exist.
pub async fn create_database(engine: Engine, path: &Path) {
    spelaion_core::create_file_database(engine, path)
        .await
        .unwrap_or_else(|e| panic!("could not create {}: {e}", path.display()));
}

/// The same, for a file that may already be there: it is created only if it is missing.
pub async fn ensure_database(engine: Engine, path: &Path) {
    if !path.exists() {
        create_database(engine, path).await;
    }
}
