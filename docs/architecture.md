# SpelaionDB architecture

A summary of how the pieces fit together. The reasons behind each decision are in `docs/adr/`.

```
            ┌──────────────────────┐        ┌─────────────────────┐
            │  Desktop app         │        │  `spelaiondb` CLI   │
            │  Tauri 2 + React     │        │  `spelaiondb mcp`   │◄── agent (stdio)
            └──────────┬───────────┘        └──────────┬──────────┘
                       │ thin adapter                  │ thin adapter
                       ▼                               ▼
            ┌─────────────────────────────────────────────────────┐
            │ spelaion-core  (no Tauri)                           │
            │  sessions · execution · results (Arrow) ·           │
            │  statement classification · permissions · audit     │
            ├──────────────┬───────────────┬──────────────────────┤
            │ spelaion-ssh │ driver-api    │ spelaion-config      │
            │ (russh)      │ + drivers     │ (TOML + secrets)     │
            ├──────────────┴───────────────┴──────────────────────┤
            │ spelaion-store (local SQLite: history, cache,       │
            │ audit, snippets)                                    │
            └─────────────────────────────────────────────────────┘
```

## Key flows

**Opening a connection**
1. `spelaion-config` loads `.spelaion/` and the personal directory, and resolves the
   connection (variables, paths, secrets). Repository connections require trusting it.
2. If the password is `prompt` (or the keychain does not have it yet), the UI asks for it.
3. `spelaion-core::open_connection` opens the SSH tunnel if there is one (`spelaion-ssh`), and
   the driver connects to its local end with `connect_addr`, keeping the real host name to
   verify TLS.
4. The tunnel lives as long as the session; closing the connection closes it.

**Running a query from the app**
1. The UI sends `Execute { tab, sql }` to the core.
2. The core splits the SQL according to the dialect and runs it on that tab's connection
   (which goes through the shared SSH tunnel if there is one). With auto-commit off, or when
   smart commit meets the first write, the core opens a transaction first; after every
   statement it asks the driver whether one is open, and tells the tab (ADR-031).
3. Rows arrive streamed, are converted to Arrow batches and stored in the core's result
   store (in memory up to a budget; then on disk).
4. The UI requests windows of rows while scrolling and receives them as binary Arrow IPC.
   Sorting by a column keeps a row permutation in the core, so windows, copies and exports
   all see the same order.

**Column masking (ADR-051)**
A connection's masking rules (its `[masking]`, the user's `user.toml`, its connection type)
are compiled into the session. Every result the session stores is masked as its batches
arrive: masked columns become `••••••` in the stored batches and their originals are kept
aside, read only to reveal one cell. Windows, sorting, filters, Ctrl+F, the calc panel,
copies and exports never see a value; streaming exports, column statistics, the data search
and agents' reads apply the same rules. It is not access control: aliases get past it.

**Execution plans**
The core's `plan` module owns the normalized tree, metrics, generic findings, bounded document
store and raw/CSV file behavior. The desktop executes a core-prepared request on the selected
session, using the underlying statement for safeguards and transaction intent. CLI file
reads and MCP normalized pages use the same model; adapters never parse an engine's plan.
See [execution plans](execution-plans.md) for metric semantics, capabilities and limits.

**A query from an agent (v0.1)**
1. The agent launches `spelaiondb mcp --profile <profile>`.
2. `spelaiondb mcp` loads the same config and keychain, and only sees the connections with
   `mcp.exposed = true`.
3. Every `run_query` goes through the classifier, runs in a read-only transaction with
   limits, and is recorded in the audit log.
4. On connections with `mcp.writes = "allow"`, `execute_write` runs one checked write in its
   own transaction, in a session separate from the reads, also audited (ADR-019).

## Phases
- **Phase 0** (done, 2026-11-02): risk experiments for the conditional decisions; ADR-003,
  ADR-007 and ADR-010 record what they measured.
- **v0.1:** PostgreSQL + SQLite, multi-hop SSH with agent, editor, read-only grid,
  explorer, history, read-only `spelaiondb mcp`.
- **v0.2:** MySQL/MariaDB (in the MCP since ADR-020), result editing,
  export, automatic reconnection, 2FA and certificates, `~/.ssh/config` import.
- **v0.3:** layered config and secret managers, the app serves MCP with confirmed writes,
  automatic updates (prepared and signed, ADR-021; on once releases are public), and
  installer signing.
- **v0.4+:** ERD, plan visualizer, more engines, external drivers and a JDBC bridge.
