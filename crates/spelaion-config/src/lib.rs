//! Version-controlled SpelaionDB configuration (ADR-009).
//!
//! - **Repository** (`.spelaion/connections/`, `.spelaion/tunnels/`): shared in git, without
//!   secrets. One TOML file per connection or tunnel; subdirectories are folders.
//! - **User** (personal configuration directory): `user.toml` with personal variables
//!   (`${user.…}`) and personal connections that never go to git.
//!
//! Secrets are stored by reference (`keychain`, `env:`, `op://`, `cmd:`, `prompt`) and are
//! only resolved when connecting. A repository's connections require trusting it before
//! anything is resolved ([`TrustStore`]).

mod accounts;
mod check;
mod conn_url;
mod copy;
mod dbeaver;
mod draft;
mod edit;
mod engine_change;
mod error;
mod load;
pub mod mcp_clients;
mod migrate;
mod model;
mod name_pattern;
mod relocate;
mod resolve;
mod secrets;
mod trust;
mod tunnel_draft;
mod types;
mod types_edit;
mod vars;

pub use accounts::{connection_account, proxy_account, tunnel_account};
pub use check::{Finding, Severity, check_workspace};
pub use conn_url::{UrlConnection, UrlError, parse_connection_url};
pub use copy::{
    ClipboardConnection, Duplicated, MAX_CLIPBOARD_TEXT, Pasted, connection_clipboard_text,
    duplicate_connection, paste_connection,
};
pub use dbeaver::{
    DbeaverConnection, DbeaverError, DbeaverProxy, DbeaverSsh, ImportPlan, dbeaver_workspace,
    import_plan, read_dbeaver,
};
pub use draft::{
    CloudToken, ConnectionDraft, KubernetesDraft, PasswordChoice, add_connection,
    connection_revision, connections_dir, delete_connection, normalize_tags, slug,
    update_connection, update_connection_at_revision,
};
pub use edit::{
    CONNECTION_SCHEMA_URL, create_connection, remove_value, set_tunnel_ports, set_user_vars,
    set_value,
};
pub use engine_change::EngineChange;
pub use error::{ConfigError, ResolveError};
pub use load::{
    CONNECTION_TYPES_FILE, Config, ConnectionEntry, ConnectionId, Source, TunnelEntry, TypesLayer,
    WORKSPACE_DIR, default_user_dir, ensure_private_dir, find_workspace, load, valid_type_id,
    workspace_dir,
};
pub use migrate::{LEGACY_WORKSPACE_DIR, legacy_user_dir, migrate_user_dir, state_db};
pub use model::{
    ConnectionFile, ConnectionTypeConfig, ConnectionTypesFile, Engine, HopConfig, KubernetesConfig,
    McpConfig, ProxyConfig, ProxyType, SCHEMA_VERSION, SecretSpec, SshAuth, TlsConfig,
    TlsModeConfig, Tone, TunnelFile, TunnelPreferences, UserFile, WritePolicy,
};
pub use name_pattern::{
    DEFAULT_NAME_PATTERN, MAX_NAME_PATTERN, NAME_VARIABLES, NamePattern, NamePatternError,
    NameValues,
};
pub use relocate::{Moved, move_connection};
pub use resolve::{
    LocalPort, Password, ResolvedConnection, ResolvedHop, ResolvedKubernetes, ResolvedProxy,
    ResolvedTunnel, Resolver, TunnelSettings,
};
pub use secrets::{
    KEYCHAIN_SERVICE, KEYCHAIN_UNAVAILABLE, SecretBackend, SecretRef, SystemBackend,
};
pub use trust::{
    CONNECTION_TYPES_ITEM, FieldChange, ItemChange, KNOWN_HOSTS_ITEM, TrustStore, connection_item,
    team_known_hosts, tunnel_item,
};
pub use tunnel_draft::{
    HopDraft, ProxyDraft, TUNNEL_SCHEMA_URL, TunnelDraft, TunnelDraftOptions, TunnelPasswords,
    add_tunnel, delete_tunnel, tunnel_revision, tunnel_users, tunnels_dir, update_tunnel,
    update_tunnel_at_revision,
};
pub use types::{ConnectionType, ConnectionTypes, PRODUCTION, Safeguards, type_id};
pub use types_edit::{remove_personal_type, set_personal_type};

/// JSON Schema of each file type, for validation and autocompletion in the editor.
/// They are stored in `schema/` and a test checks they are up to date.
pub fn json_schemas() -> Vec<(&'static str, serde_json::Value)> {
    let to_json = |s: schemars::Schema| serde_json::to_value(s).expect("schema serializable");
    vec![
        ("connection", to_json(schemars::schema_for!(ConnectionFile))),
        ("tunnel", to_json(schemars::schema_for!(TunnelFile))),
        ("user", to_json(schemars::schema_for!(UserFile))),
        (
            "connection-types",
            to_json(schemars::schema_for!(ConnectionTypesFile)),
        ),
    ]
}
