//! Format of the configuration files (ADR-009). Changing something here changes the format
//! teams store in git: the JSON Schema (`schema/`) must be regenerated and, if it is not
//! compatible, `schema_version` must be bumped.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Format version understood by this version of SpelunkDB.
pub const SCHEMA_VERSION: u32 = 1;

/// A connection: `.spelunk/connections/<folders>/<id>.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConnectionFile {
    /// Format version of this file.
    pub schema_version: u32,
    /// Display name. If missing, the identifier (the file path) is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub engine: Engine,
    /// Environment label (`dev`, `staging`, `prod`…). `prod` tightens the MCP permissions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Database; for SQLite and DuckDB, the file path (relative to the repository root).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Reference to the secret, never the value: `{ secret = "keychain" }`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretSpec>,
    /// Name of a tunnel in `.spelunk/tunnels/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tunnel: Option<String>,
    /// Reach the database with `kubectl port-forward` to a resource of a cluster (ADR-015):
    /// `port` is then the resource's port and `host` is only the name TLS checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubernetes: Option<KubernetesConfig>,
    /// Open read-only (native in SQLite).
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "TlsConfig::is_default")]
    pub tls: TlsConfig,
    #[serde(default, skip_serializing_if = "McpConfig::is_default")]
    pub mcp: McpConfig,
}

fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Engine {
    Postgres,
    Sqlite,
    Mysql,
    Mariadb,
    Duckdb,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Postgres => "postgres",
            Engine::Sqlite => "sqlite",
            Engine::Mysql => "mysql",
            Engine::Mariadb => "mariadb",
            Engine::Duckdb => "duckdb",
        }
    }

    /// Embedded engines: `database` is a file, and there is no server, user or tunnel.
    pub fn is_file(self) -> bool {
        match self {
            Engine::Sqlite | Engine::Duckdb => true,
            Engine::Postgres | Engine::Mysql | Engine::Mariadb => false,
        }
    }

    pub fn default_port(self) -> Option<u16> {
        match self {
            Engine::Postgres => Some(5432),
            Engine::Sqlite | Engine::Duckdb => None,
            Engine::Mysql | Engine::Mariadb => Some(3306),
        }
    }
}

/// Reference to a secret. Accepted forms:
/// `keychain`, `keychain:<name>`, `env:<VARIABLE>`, `op://<vault>/<item>/<field>`,
/// `cmd:<command>` and `prompt` (asked when connecting).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretSpec {
    pub secret: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    #[serde(default)]
    pub mode: TlsModeConfig,
    /// Certificate (PEM) of the authority, relative to the repository root or with `~/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca: Option<String>,
}

impl TlsConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Same names as libpq's `sslmode`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TlsModeConfig {
    Disable,
    #[default]
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

/// What an agent can do with this connection (ADR-008).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    /// Visible to agents. Off by default.
    #[serde(default)]
    pub exposed: bool,
    /// Maximum rows per response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_rows: Option<u32>,
    /// Write policy. On `env = "prod"` connections it is always `deny`.
    #[serde(default)]
    pub writes: WritePolicy,
}

impl McpConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum WritePolicy {
    #[default]
    Deny,
    Confirm,
    Allow,
}

/// An SSH tunnel: `.spelunk/tunnels/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TunnelFile {
    pub schema_version: u32,
    /// Hops in order; the last one is the one that reaches the database.
    pub hops: Vec<HopConfig>,
}

/// The cluster resource a connection reaches through `kubectl port-forward` (ADR-015).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct KubernetesConfig {
    /// Context of the kubeconfig; its current context when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// The context's namespace when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    /// `svc/<name>`, `pod/<name>` or `deployment/<name>`.
    pub resource: String,
    /// A kubeconfig file instead of the default one (`KUBECONFIG` or `~/.kube/config`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubeconfig: Option<String>,
    /// The `kubectl` executable; `kubectl` on the PATH when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kubectl: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HopConfig {
    pub host: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    pub user: String,
    /// Methods to try, in order. Defaults to `["agent"]`.
    #[serde(default = "default_auth")]
    pub auth: Vec<SshAuth>,
    /// A specific private key (it also filters the agent's keys).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<SecretSpec>,
    /// External command used as the transport, like `ProxyCommand` (`%h` and `%p`).
    /// Requires trusting the repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
}

fn default_auth() -> Vec<SshAuth> {
    vec![SshAuth::Agent]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SshAuth {
    Agent,
    Key,
    Password,
    KeyboardInteractive,
}

/// Personal configuration: `<user config>/spelunkdb/user.toml`. Never goes to git.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UserFile {
    #[serde(default = "current_version")]
    pub schema_version: u32,
    /// Values for `${user.<name>}` in the repository's connections.
    #[serde(default)]
    pub vars: BTreeMap<String, String>,
}

fn current_version() -> u32 {
    SCHEMA_VERSION
}
