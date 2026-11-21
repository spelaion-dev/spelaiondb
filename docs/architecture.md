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
   Sorting by one or several columns keeps a row permutation in the core, so windows, copies
   and exports all see the same order. A table data tab and a paged query (ADR-054) are
   sorted on the server instead: the core writes the `ORDER BY` and the page is read again.

**Column masking (ADR-051)**
A connection's masking rules (its `[masking]`, the user's `user.toml`, its connection type)
are compiled into the session. Every result the session stores is masked as its batches
arrive: masked columns become `••••••` in the stored batches and their originals are kept
aside, read only to reveal one cell. Windows, sorting, filters, Ctrl+F, the calc panel,
copies and exports never see a value; streaming exports, column statistics, the data search
and agents' reads apply the same rules. Drivers report the table column each result column
reads where the engine can (ADR-052), so an alias stays masked; an expression gets past it,
so masking is not access control.

**Execution plans**
The core's `plan` module owns the normalized tree, metrics, generic findings, bounded document
store and raw/CSV file behavior. The desktop executes a core-prepared request on the selected
session, using the underlying statement for safeguards and transaction intent. CLI file
reads and MCP normalized pages use the same model; adapters never parse an engine's plan.
See [execution plans](execution-plans.md) for metric semantics, capabilities and limits.

**A query from an agent (v0.1)**
1. The agent launches `spelaiondb mcp --profile <profile>`.
2. `spelaiondb mcp` loads the same config and keychain, and only sees the connections with
   `mcp.exposed = true`. It reads the configuration and the trust store again at the start of
   every tool call, so the user's changes apply to a server that is already running.
3. Every `run_query` goes through the classifier, runs in a read-only transaction with
   limits, and is recorded in the audit log.
4. On connections with `mcp.writes = "allow"`, `execute_write` runs one checked write in its
   own transaction, in a session separate from the reads, also audited (ADR-019).
5. A refused or failed call answers its sentence and, as structured content, the core's code
   for it and whether running it again makes sense (`spelaion-core::agent_state`, SDB-374);
   the audit log records the same code.

**The window's Content Security Policy (SDB-127)**
The window shows a lot of text it does not control (cell values, names, comments, server
messages, agents' statements), and any script that ran in it could call every command of the
app. All of it is rendered as text today; the policy is the layer that keeps one future
mistake, or a compromised dependency, from running script there. It is
`apps/desktop/src-tauri/tauri.conf.json`'s `app.security.csp`, which the release build keeps
(`tauri.release.conf.json` is merged over it and does not name it). Tauri adds the nonces and
hashes of its own scripts. Everything starts from `default-src 'self'` (the app's own files),
with `object-src`, `frame-src`, `base-uri` and `form-action` set to `'none'`; only these are
relaxed, each for one thing:

- `script-src 'self'`: no `'unsafe-inline'` and no `'unsafe-eval'`; nothing in the app needs either.
- `style-src 'unsafe-inline'`: CodeMirror writes its themes into `<style>` elements it creates
  (`style-mod`), and components set `style` attributes. CSS cannot run script; it is the one
  inline relaxation.
- `img-src data:`: the cell viewer shows images as `data:` URLs of the types the core recognised
  by their bytes (never SVG), and the chart and ER diagram exports draw their SVG through an
  `Image` with a `data:image/svg+xml` URL.
- `img-src https: http://localhost:* http://127.0.0.1:*`: the map's tiles (ADR-045) come from
  the server the user chose in Settings, whose address the core only accepts over HTTPS, or
  HTTP to `localhost` or `127.0.0.1` (`spelaion-store`'s `check_tile_server`; not `[::1]`, which
  a policy cannot name). Off by default.
- `connect-src ipc: http://ipc.localhost`: Tauri's commands, `fetch_window`'s binary windows
  included (the custom `ipc:` scheme, or `http://ipc.localhost` on Windows). The app reaches no
  other address from the page: updates, plugins and every database go through the Rust side.

`devCsp` is the same for `tauri dev`, plus what Vite's dev server needs: its inline React
refresh preamble (`script-src 'unsafe-inline'`, development only) and its hot-reload socket
(`connect-src ws://localhost:1420`). Tauri adds a policy only to the files it serves itself, and
in development the window loads Vite's server directly, so `vite.config.ts` reads `devCsp` and
sends it as that server's header. The Windows smoke test fails on any refusal the policy
reports, and checks first that an injected inline script does not run.

**Opening from outside the app (SDB-397)**
`spelaion-desktop [FOLDER] [FILE.sql …] [--connection ID] [--plan FILE]`: the core reads the
words (`spelaion-core`'s `launch.rs`). A folder must exist. A file must be an existing `.sql`
file no bigger than the editor opens, then read like a picked one (ADR-036). A connection is
only selected in the explorer. What is not accepted is said in a banner. Nothing is
connected or run: the files open as tabs without a connection, and a folder goes through
repository trust (ADR-022).

One app runs per personal folder (`instance.rs`). The first listens on a local port and writes
the port and a random token, owner-only, to `desktop-instance.json`. A later launch with the
same personal folder sends its arguments and its folder there with the token, and ends. The
running window opens them as above (a folder after the same questions as the folder menu).
Keyed by the personal folder, so a second profile or the smoke test's folder never sends
anything to the user's app. No dependency was added for it. The installers register `.sql`
files (`tauri.conf.json`'s `fileAssociations`); macOS's Finder sends them as an event, read
the same way.

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
