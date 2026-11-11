# Architecture decision records (ADR)

Each ADR records a decision, why it was made and what would make us revisit it. Decisions
marked **Accepted** override any other note or conversation; those marked **Conditional**
depend on the outcome of phase 0: the risk experiments run before writing the product, whose
results each of those ADRs records as its evidence.

To change a decision the old ADR is not edited: a new one is written that supersedes it,
and the old one is marked **Superseded by ADR-XXX**.

| ADR | Decision | Status |
|---|---|---|
| [001](0001-core-in-rust.md) | Core in Rust | Conditional (phase 0) |
| [002](0002-core-boundary.md) | Core as a library independent of Tauri; security lives in the core | Accepted |
| [003](0003-tauri-shell.md) | Desktop shell: Tauri 2 | Conditional (phase 0) |
| [004](0004-no-daemon.md) | No daemon: app + standalone `spelunk mcp` | Accepted |
| [005](0005-driver-model.md) | Driver model: common contract + capabilities + `Native` values (its extensibility part superseded by ADR-041) | Accepted |
| [006](0006-low-level-drivers.md) | Low-level drivers, not `sqlx` | Accepted |
| [007](0007-in-process-ssh.md) | In-process SSH with `russh` + hops through an external command | Conditional (phase 0) |
| [008](0008-mcp.md) | MCP over stdio, profiles, defense in layers, read-only in v0.1 | Accepted |
| [009](0009-configuration-and-secrets.md) | TOML configuration, layers, secret references, sync through git | Accepted |
| [010](0010-frontend-and-results.md) | React + CodeMirror 6 + canvas grid; results in the core (Arrow) | Conditional (phase 0) |
| [011](0011-local-storage.md) | Local SQLite in WAL mode | Accepted |
| [012](0012-ci-as-reviewer.md) | CI is the code reviewer | Accepted |
| [013](0013-license-and-repo.md) | Apache-2.0 + DCO + trademark policy; monorepo (plugins have their own repositories: ADR-042) | Accepted |
| [014](0014-editing-table-data.md) | Editing in table data tabs, by primary key, in one transaction (pending changes in the core: superseded by ADR-044) | Accepted |
| [015](0015-kubernetes-tunnels.md) | Kubernetes connections through `kubectl port-forward` | Accepted |
| [016](0016-duckdb.md) | DuckDB driver: bundled, one open per file (its MCP part superseded by ADR-020) | Accepted |
| [017](0017-sql-server.md) | SQL Server driver: tiberius, T-SQL batches, cancel by Attention | Accepted |
| [018](0018-sqlite-over-ssh.md) | SQLite files on a server, through the server's `sqlite3` over SSH | Accepted |
| [019](0019-mcp-writes.md) | Agents may write, per connection (supersedes ADR-008's writes) | Accepted |
| [020](0020-mcp-engines.md) | The MCP serves MySQL, MariaDB, SQL Server and DuckDB, with each engine's barriers | Accepted |
| [021](0021-automatic-updates.md) | Automatic updates: signed, installed on the user's click, off until there is a public source | Accepted |
| [022](0022-team-sharing.md) | Sharing connections with a team: trust pinned to content, shared host keys, passwordless logins | Accepted |
| [023](0023-spelaion.md) | Rename SpelunkDB to SpelaionDB, under the Spelaion brand | Accepted |
| [024](0024-explorer-folders.md) | The explorer groups objects in folders by type: routines, sequences, triggers, types and server objects | Accepted |
| [025](0025-explorer-filters.md) | Explorer filters: personal, matched in Rust, narrowed on the server | Accepted |
| [026](0026-explorer-object-actions.md) | The explorer's object actions: SQL from the core, run only after it is shown | Accepted |
| [027](0027-proxies-and-network-profiles.md) | SOCKS and HTTP proxies go in tunnel files, and tunnel files are the network profiles | Accepted |
| [028](0028-idle-sessions.md) | Idle database sessions: keep them alive, notice when one dies, reconnect only on a click | Accepted |
| [029](0029-connection-types.md) | Connection types: a badge and safeguards per environment, decided in the core; safeguards only add up | Accepted |
| [030](0030-copying-connections-and-changing-engines.md) | Copying connections, changing their engine, and naming new ones | Accepted |
| [031](0031-transactions.md) | Transactions you can see and control: manual commit per tab, smart commit per type | Accepted |
| [032](0032-connection-test-diagnostics.md) | The connection test says which step failed, why, and what to try | Accepted |
| [033](0033-mcp-page.md) | The MCP server page: read-only mode for every agent, query rows as JSON or TOON, one-click setup of AI clients | Accepted |
| [034](0034-streaming-exports.md) | Exports that stream to a file: a new read-only run in its own session, written batch by batch | Accepted |
| [035](0035-script-runs.md) | Script runs that behave like a script runner: error modes, timeouts and server output | Accepted |
| [036](0036-sql-files-and-restored-tabs.md) | SQL files in editor tabs, and open tabs that survive a restart | Accepted |
| [037](0037-session-setup.md) | Every new session starts as its connection says: setup statements, default schema, isolation level | Accepted |
| [038](0038-importing-data.md) | Importing data into a table: values bound, statements shown first, whole batches | Accepted |
| [039](0039-importing-connections.md) | Importing connections from other clients: one flow, a reader per client | Accepted |
| [040](0040-charts-from-a-result.md) | Charts from a result: points prepared in the core, drawn as plain SVG | Accepted |
| [041](0041-engines-as-plugins.md) | Engines as plugins: every driver in its own process behind the same contract, migrated one at a time; dialects and barriers stay in the core (where ours are built and checked: superseded by ADR-042) | Accepted (not implemented) |
| [042](0042-plugin-repositories.md) | Our plugins live in their own repositories; this one keeps the contract and pins, by hash, the builds it ships and tests | Accepted (not implemented) |
| [043](0043-searching-the-database.md) | Searching the database: names from the explorer's catalog, values bound in a read-only session | Accepted |
| [044](0044-pending-edits-in-the-core.md) | Pending edits live in the core, with bulk edits of a selection and undo / redo (supersedes ADR-014's pending changes in the UI) | Accepted |
| [045](0045-geometry-on-a-map.md) | Geometry values on a map: GeoJSON from the core, drawn as SVG, tiles only when the user names a server | Accepted |
| [046](0046-er-diagrams.md) | ER diagrams: laid out in the core from the explorer's catalog, drawn as plain SVG | Accepted |
| [047](0047-structure-editing-with-forms.md) | Structure editing with forms: one script from the core, applied only as it was shown; SQLite rebuilds the table | Accepted |
| [048](0048-users-and-privileges.md) | Users and privileges: forms that write reviewed SQL, with the password kept out of everything but the statement | Accepted |
| [049](0049-backup-and-restore.md) | Backup and restore through the tunnel: the vendors' own tools, started by the core with an argument list and the password in their environment | Accepted |
| [050](0050-kubernetes-and-ssm-network-profiles.md) | Network profiles reach Kubernetes clusters (reusable forwards) and AWS SSM managed nodes | Accepted |
| [051](0051-column-masking.md) | Column masking: values replaced where results enter the core, revealed one cell at a time; not access control | Accepted (amended by ADR-052) |
| [052](0052-masking-follows-the-origin-column.md) | Column masking also follows the table column a result column reads, where the engine reports it, so an alias does not unmask it | Accepted |
