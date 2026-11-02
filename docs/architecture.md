# SpelunkDB architecture

A summary of how the pieces fit together. The reasons behind each decision are in `docs/adr/`.

```
            ┌──────────────────────┐        ┌─────────────────────┐
            │  Desktop app         │        │  `spelunk` CLI      │
            │  Tauri 2 + React     │        │  `spelunk mcp`      │◄── agent (stdio)
            └──────────┬───────────┘        └──────────┬──────────┘
                       │ thin adapter                  │ thin adapter
                       ▼                               ▼
            ┌─────────────────────────────────────────────────────┐
            │ spelunk-core  (no Tauri)                            │
            │  sessions · execution · results (Arrow) ·           │
            │  statement classification · permissions · audit     │
            ├──────────────┬───────────────┬──────────────────────┤
            │ spelunk-ssh  │ driver-api    │ spelunk-config       │
            │ (russh)      │ + drivers     │ (TOML + secrets)     │
            ├──────────────┴───────────────┴──────────────────────┤
            │ spelunk-store (local SQLite: history, cache,        │
            │ audit, snippets)                                    │
            └─────────────────────────────────────────────────────┘
```

## Key flows

**Opening a connection**
1. `spelunk-config` loads `.spelunk/` and the personal directory, and resolves the
   connection (variables, paths, secrets). Repository connections require trusting it.
2. If the password is `prompt` (or the keychain does not have it yet), the UI asks for it.
3. `spelunk-core::open_connection` opens the SSH tunnel if there is one (`spelunk-ssh`), and
   the driver connects to its local end with `connect_addr`, keeping the real host name to
   verify TLS.
4. The tunnel lives as long as the session; closing the connection closes it.

**Running a query from the app**
1. The UI sends `Execute { tab, sql }` to the core.
2. The core splits the SQL according to the dialect and runs it on that tab's connection
   (which goes through the shared SSH tunnel if there is one).
3. Rows arrive streamed, are converted to Arrow batches and stored in the core's result
   store (in memory up to a budget; then on disk).
4. The UI requests windows of rows while scrolling and receives them as binary Arrow IPC.

**A query from an agent (v0.1)**
1. The agent launches `spelunk mcp --profile <profile>`.
2. `spelunk mcp` loads the same config and keychain, and only sees the connections with
   `mcp.exposed = true`.
3. Every `run_query` goes through the classifier, runs in a read-only transaction with
   limits, and is recorded in the audit log.

## Phases
- **Phase 0** (current): risk experiments in `spikes/`.
- **v0.1:** PostgreSQL + SQLite, multi-hop SSH with agent, editor, read-only grid,
  explorer, history, read-only `spelunk mcp`.
- **v0.2:** MySQL/MariaDB, result editing, export, automatic reconnection, 2FA and
  certificates, `~/.ssh/config` import.
- **v0.3:** layered config and secret managers, the app serves MCP with confirmed writes,
  signing and auto-update.
- **v0.4+:** ERD, plan visualizer, more engines, external drivers and a JDBC bridge.
