# SpelunkDB — rules for agents

An open-source (Apache-2.0) desktop database client, with first-class SSH tunnels and an
MCP server. **All the code is written by an agent and nobody reviews it line by line**:
the compiler, the tests and CI are the only reviewer. Write accordingly.

## Before touching anything
- Read `docs/architecture.md` and the ADRs in `docs/adr/`. ADRs rule: if a change
  contradicts them, do not make it; propose a new ADR that supersedes the old one.
- Current state: **v0.2**. Done: PostgreSQL (with TLS), MySQL/MariaDB, SQL Server, SQLite
  and DuckDB drivers, configuration, SSH tunnels, core, local store, `spelunk mcp` (read-only, writes per connection) and the
  `spelunk` binary, and the first version of the desktop app (`apps/desktop`).
  `spikes/` is throwaway phase 0 code: it is never imported from `crates/`.
- Everything in the repository is in English: code, comments, docs, UI text, tests and
  commit messages.

## Workspace map
- `crates/spelunk-driver-api`: driver contract (ADR-005). Results in Arrow with
  `spelunk.logical` / `spelunk.native_type` metadata per column; `Value` for single cells.
- `crates/spelunk-driver-postgres`: extended protocol + our own binary decoding
  (`decode.rs`). Each type's text must be identical to `psql`'s: the type zoo
  (`tests/postgres.rs`) checks it. A new type is added there first.
- `crates/spelunk-driver-mysql`: MySQL and MariaDB (`mysql_async`, text protocol, one actor
  per connection). Its type zoo (`tests/mysql.rs`) compares every type with the server's
  text, on both servers.
- `crates/spelunk-driver-sqlite`: `rusqlite` in one thread per connection (an actor).
  Dynamic typing resolved in `exec.rs` (see ADR-006). Its tests need nothing external.
  `remote.rs` opens a file on a server (ADR-018) by running that server's `sqlite3 -safe`
  through `ConnectParams::remote` (an SSH exec channel on the tunnel's last hop,
  `spelunk-ssh::Tunnel::exec`): it must never let a dot-command or an incomplete statement
  reach the shell. Catalog queries are shared in `catalog.rs`. Tests against
  `testing/docker/sqlitehost` with `SPELUNK_TEST_SSH`.
- `crates/spelunk-driver-sqlserver`: `tiberius` over rustls, one actor task per connection
  (ADR-017). T-SQL batches (`GO`, `BEGIN … END`) split in `dialect.rs`; a batch may return
  several result sets. Cancel is a TDS Attention through a patched tiberius in
  `vendor/tiberius` (read its `PATCHED.md` before upgrading). Tests against the Docker SQL
  Server with `SPELUNK_TEST_MSSQL=127.0.0.1:11433`.
- `crates/spelunk-driver-duckdb`: `duckdb` (bundled) in one thread per connection, one open
  per file shared by clones (ADR-016). Arrow from DuckDB converted in `exec.rs`; nested types
  written as DuckDB's text in `format.rs`. Its type zoo (`tests/duckdb.rs`) compares every
  type with `CAST(… AS VARCHAR)`; it needs nothing external.
- `crates/spelunk-config`: TOML files from `.spelunk/` and the personal directory,
  variables, secret references and repository trust (ADR-009). If you change `model.rs`,
  regenerate the schema: `SPELUNK_UPDATE_SCHEMA=1 cargo test -p spelunk-config`.
  `draft.rs` turns the add/edit connection form into a validated file (secrets go to the
  keychain, never to the file); editing touches only the changed keys with `toml_edit`.
  `tunnel_draft.rs` does the same for SSH tunnels (hops matched by position; a stored hop
  password is forgotten when that hop points to another server). `dbeaver.rs` reads DBeaver's
  connections and saved credentials for the import (passwords only as `Secret`, to the keychain).
- `crates/spelunk-ssh`: tunnels with `russh` (ADR-007). It does not know the configuration:
  it receives a `TunnelSpec`. Dialogs go through the `Interaction` trait. Tests against the
  Docker bastions with `SPELUNK_TEST_SSH=127.0.0.1:2221`.
  Kubernetes (ADR-015) is not a tunnel file but a connection's `[kubernetes]` section:
  `spelunk-core::kube` runs `kubectl port-forward` and lists contexts, namespaces, names and
  ports for the form's Kubernetes tab; tests against a kind cluster with
  `SPELUNK_TEST_K8S=<kubeconfig>` (see `testing/k8s`).
- `crates/spelunk-core`: `open_connection` (resolved configuration → tunnel from the
  `TunnelPool` if any → driver → `Session`), `Session` (splits and runs scripts, stops at
  the first error) and `ResultStore` (results in the core, served in Arrow IPC windows).
  `catalog.rs` lists the engines of the "add connection" picker (planned ones included).
  `classify.rs` and `readonly.rs` are the read-only mode barriers (ADR-008): any change
  there needs tests that try to get around them. `guard.rs` lists the statements the app
  must confirm before running (DELETE/UPDATE without WHERE, DROP, TRUNCATE, production
  writes); `run_script` refuses unconfirmed risky scripts. `sort.rs` orders stored results
  (`results.rs` combines the sort, the text filter and the column filters of `filter.rs` into
  one view)
  and `cursor.rs` finds the statement under the editor's cursor. `table_data.rs` writes and checks
  the paged SELECT of the table data tab (the filter typed by the user cannot change it).
  `edit.rs` writes and applies the edits of a table data tab (ADR-014: primary key from the
  database, one transaction, one row per statement).
  `write.rs` holds the barriers and the transaction of agents' writes (ADR-019, ADR-020: one
  statement, never destructive, only the kinds on its allow list). Each engine's read barrier
  is in `readonly.rs` (ADR-020) and is tested in `tests/agents_engines.rs`; like
  `classify.rs`, any change there needs tests that try to get around it.
- `crates/spelunk-store`: local SQLite (audit, query history, saved queries and personal settings;
  later the schema cache). `spelunk-core::QueryHistory` owns its desktop actor and wraps
  session execution to record each executed statement before its completion is delivered.
- `crates/spelunk-mcp` + `apps/cli`: `spelunk mcp` and utilities (`connections`, `trust`,
  `audit`). In `spelunk mcp`, stdout is the protocol: no `println!`. Reads and writes
  (`execute_write`, only with `[mcp] writes = "allow"`) use separate sessions.
- `apps/desktop`: Tauri 2 + React app. `src-tauri` is a thin adapter over the core; the
  command types are generated into `src/bindings.ts`
  (`cargo run -p spelunk-desktop --features bindings --bin export-bindings`; CI checks it is up to date).
  `fetch_window` is the only command outside tauri-specta (binary Arrow, cells already as
  text). Our own canvas grid (`ResultGrid.tsx`). Smoke test against the real app:
  `node apps/desktop/e2e/smoke.mjs` (after `npx tauri build --no-bundle`).
  Automatic updates (ADR-021): `src-tauri/src/update.rs` (Tauri's updater plugin; its
  `ENDPOINTS` stay empty until releases have a public home, and then the app says updates are
  unavailable). Update artifacts are signed only in the release workflow
  (`tauri.release.conf.json`, secret `TAURI_SIGNING_PRIVATE_KEY`); never commit a private key.
  Installers: `npx tauri build` (NSIS + MSI on Windows). Releases: pushing a tag `vX.Y.Z`
  runs `.github/workflows/release.yml` (installers for every OS, the `spelunk` CLI, notes from
  `docs/releases/vX.Y.Z.md` plus every commit since the previous tag, from
  `scripts/release-notes.sh`, so commit subjects must read well in a changelog); the tag must
  match the version in `Cargo.toml` and `tauri.conf.json`.

## Branches
- Day-to-day work goes to `dev` (push freely: CI does not run on its pushes, the repository
  is private and Actions minutes cost). Run the local checks below before each commit.
- To release, open a pull request `dev` → `main`: CI runs on it, and it is merged only when
  green. Then tag `main` with `vX.Y.Z`. CI can also be started by hand on `dev`
  (`gh workflow run CI --ref dev`), for example for a change that touches macOS or Windows.

## Non-negotiable rules
- `spelunk-core` does not depend on Tauri. The app, the CLI and the MCP are thin adapters.
- All security lives in the core: statement splitting and classification, permissions,
  audit and secrets. Never in React or in a Tauri command.
- Secrets are never written to logs, config files, history or MCP responses.
- After reconnecting a tunnel or a connection, nothing is ever re-run automatically.
- A changed SSH fingerprint is a blocking error, never a warning accepted with one click.

## Rust style
- Concurrency with actors: tokio tasks that communicate through channels with owned data.
  Do not hold `Mutex` guards across `.await`.
- No lifetimes in public APIs; use owned types (`String`, `Arc<…>`).
- No `unsafe` except justified FFI with a comment.
- Errors with `thiserror` in libraries and `anyhow` only in binaries.
- Exhaustive `match` over our own enums: no `_ =>` that hides new variants.

## Frontend style
- React + strict TypeScript. Types that cross from Rust are generated, not handwritten.
- Result data never goes into React state: it lives in the core and is requested in windows.

## Before calling something done
- `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`.
- Tests with a real PostgreSQL (without the variable they are skipped, and that does
  **not** count as tested):
  `docker compose -f testing/docker/compose.yaml up -d --wait` and
  `SPELUNK_TEST_PG=127.0.0.1:15432 SPELUNK_TEST_MYSQL=127.0.0.1:13306
  SPELUNK_TEST_MARIADB=127.0.0.1:13307 SPELUNK_TEST_MSSQL=127.0.0.1:11433
  SPELUNK_TEST_SSH=127.0.0.1:2221 cargo test --workspace`.
- `cargo deny check` if dependencies change.
- Frontend (`apps/desktop`): `npm run build` with no type errors and, if the UI changes,
  the smoke test `node e2e/smoke.mjs` (drives the real app over WebView2 CDP).
- If something could not be tested (for example Pageant or WebKitGTK), say so explicitly.

## Development environment
- Windows 11 is the main machine. Linux is tried with WSL (`scripts/setup-wsl.sh`) as a
  first look; definitive measurements on real Linux.
- Test databases and SSH bastions: `testing/docker` (see its README).
