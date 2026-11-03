# Architecture decision records (ADR)

Each ADR records a decision, why it was made and what would make us revisit it. Decisions
marked **Accepted** override any other note or conversation; those marked **Conditional**
depend on the outcome of phase 0 (`spikes/`).

To change a decision the old ADR is not edited: a new one is written that supersedes it,
and the old one is marked **Superseded by ADR-XXX**.

| ADR | Decision | Status |
|---|---|---|
| [001](0001-core-in-rust.md) | Core in Rust | Conditional (phase 0) |
| [002](0002-core-boundary.md) | Core as a library independent of Tauri; security lives in the core | Accepted |
| [003](0003-tauri-shell.md) | Desktop shell: Tauri 2 | Conditional (phase 0) |
| [004](0004-no-daemon.md) | No daemon: app + standalone `spelunk mcp` | Accepted |
| [005](0005-driver-model.md) | Driver model: common contract + capabilities + `Native` values | Accepted |
| [006](0006-low-level-drivers.md) | Low-level drivers, not `sqlx` | Accepted |
| [007](0007-in-process-ssh.md) | In-process SSH with `russh` + hops through an external command | Conditional (phase 0) |
| [008](0008-mcp.md) | MCP over stdio, profiles, defense in layers, read-only in v0.1 | Accepted |
| [009](0009-configuration-and-secrets.md) | TOML configuration, layers, secret references, sync through git | Accepted |
| [010](0010-frontend-and-results.md) | React + CodeMirror 6 + canvas grid; results in the core (Arrow) | Conditional (phase 0) |
| [011](0011-local-storage.md) | Local SQLite in WAL mode | Accepted |
| [012](0012-ci-as-reviewer.md) | CI is the code reviewer | Accepted |
| [013](0013-license-and-repo.md) | Apache-2.0 + DCO + trademark policy; monorepo | Accepted |
| [014](0014-editing-table-data.md) | Editing in table data tabs, by primary key, in one transaction | Accepted |
| [015](0015-kubernetes-tunnels.md) | Kubernetes connections through `kubectl port-forward` | Accepted |
| [016](0016-duckdb.md) | DuckDB driver: bundled, one open per file (its MCP part superseded by ADR-020) | Accepted |
| [017](0017-sql-server.md) | SQL Server driver: tiberius, T-SQL batches, cancel by Attention | Accepted |
| [018](0018-sqlite-over-ssh.md) | SQLite files on a server, through the server's `sqlite3` over SSH | Accepted |
| [019](0019-mcp-writes.md) | Agents may write, per connection (supersedes ADR-008's writes) | Accepted |
| [020](0020-mcp-engines.md) | The MCP serves MySQL, MariaDB, SQL Server and DuckDB, with each engine's barriers | Accepted |
| [021](0021-automatic-updates.md) | Automatic updates: signed, installed on the user's click, off until there is a public source | Accepted |
| [022](0022-team-sharing.md) | Sharing connections with a team: trust pinned to content, shared host keys, passwordless logins | Accepted |
