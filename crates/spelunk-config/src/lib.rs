//! Version-controlled SpelunkDB configuration (ADR-009).
//!
//! - **Repository** (`.spelunk/connections/`, `.spelunk/tunnels/`): shared in git, without
//!   secrets. One TOML file per connection or tunnel; subdirectories are folders.
//! - **User** (personal configuration directory): `user.toml` with personal variables
//!   (`${user.…}`) and personal connections that never go to git.
//!
//! Secrets are stored by reference (`keychain`, `env:`, `op://`, `cmd:`, `prompt`) and are
//! only resolved when connecting. A repository's connections require trusting it before
//! anything is resolved ([`TrustStore`]).

mod accounts;
mod conn_url;
mod dbeaver;
mod draft;
mod edit;
mod error;
mod load;
mod model;
mod resolve;
mod secrets;
mod trust;
mod tunnel_draft;
mod vars;

pub use accounts::{connection_account, tunnel_account};
pub use conn_url::{UrlConnection, UrlError, parse_connection_url};
pub use dbeaver::{
    DbeaverConnection, DbeaverError, DbeaverSsh, ImportPlan, dbeaver_workspace, import_plan,
    read_dbeaver,
};
pub use draft::{
    CloudToken, ConnectionDraft, KubernetesDraft, PasswordChoice, add_connection, connections_dir,
    delete_connection, slug, update_connection,
};
pub use edit::{CONNECTION_SCHEMA_URL, create_connection, remove_value, set_value};
pub use error::{ConfigError, ResolveError};
pub use load::{
    Config, ConnectionEntry, ConnectionId, Source, TunnelEntry, WORKSPACE_DIR, default_user_dir,
    find_workspace, load,
};
pub use model::{
    ConnectionFile, Engine, HopConfig, KubernetesConfig, McpConfig, SCHEMA_VERSION, SecretSpec,
    SshAuth, TlsConfig, TlsModeConfig, TunnelFile, UserFile, WritePolicy,
};
pub use resolve::{
    Password, ResolvedConnection, ResolvedHop, ResolvedKubernetes, ResolvedTunnel, Resolver,
};
pub use secrets::{KEYCHAIN_SERVICE, SecretBackend, SecretRef, SystemBackend};
pub use trust::{
    FieldChange, ItemChange, KNOWN_HOSTS_ITEM, TrustStore, connection_item, team_known_hosts,
    tunnel_item,
};
pub use tunnel_draft::{
    HopDraft, TUNNEL_SCHEMA_URL, TunnelDraft, add_tunnel, delete_tunnel, tunnels_dir, update_tunnel,
};

/// JSON Schema of each file type, for validation and autocompletion in the editor.
/// They are stored in `schema/` and a test checks they are up to date.
pub fn json_schemas() -> Vec<(&'static str, serde_json::Value)> {
    let to_json = |s: schemars::Schema| serde_json::to_value(s).expect("schema serializable");
    vec![
        ("connection", to_json(schemars::schema_for!(ConnectionFile))),
        ("tunnel", to_json(schemars::schema_for!(TunnelFile))),
        ("user", to_json(schemars::schema_for!(UserFile))),
    ]
}
