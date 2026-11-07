# SpelaionDB — rules for agents

An open-source (Apache-2.0) desktop database client, with first-class SSH tunnels and an
MCP server. **All the code is written by an agent and nobody reviews it line by line**:
the compiler, the tests and CI are the only reviewer. Write accordingly.

## Before touching anything
- Read `docs/architecture.md` and the ADRs in `docs/adr/`. ADRs rule: if a change
  contradicts them, do not make it; propose a new ADR that supersedes the old one.
- Current state: **v0.5**. Done: PostgreSQL (with TLS), MySQL/MariaDB, SQL Server, SQLite
  and DuckDB drivers, configuration, SSH tunnels, core, local store, `spelaiondb mcp` (read-only, writes per connection) and the
  `spelaiondb` binary, and the first version of the desktop app (`apps/desktop`).
  `spikes/` is throwaway phase 0 code: it is never imported from `crates/`.
- Everything in the repository is in English: code, comments, docs, UI text, tests and
  commit messages.

## Workspace map
- `crates/spelaion-driver-api`: driver contract (ADR-005). Results in Arrow with
  `spelaion.logical` / `spelaion.native_type` metadata per column; `Value` for single cells.
  The explorer's folders (ADR-024): `list_objects` (relations), `list_schema_objects`,
  `list_server_objects` and `object_definition`, the last three in each driver's `objects.rs`
  (SQLite's in `catalog.rs`); each driver's test file checks them against its engine.
- `crates/spelaion-driver-postgres`: extended protocol + our own binary decoding
  (`decode.rs`). TLS (and client certificates, ADR-022) in `tls.rs`. Each type's text must be identical to `psql`'s: the type zoo
  (`tests/postgres.rs`) checks it. A new type is added there first.
- `crates/spelaion-driver-mysql`: MySQL and MariaDB (`mysql_async`, text protocol, one actor
  per connection). `mysql_async` is patched in `vendor/mysql_async` so `verify-ca` skips the
  name check (read its `PATCHED.md` before upgrading). Its type zoo (`tests/mysql.rs`) compares every type with the server's
  text, on both servers.
- `crates/spelaion-driver-sqlite`: `rusqlite` in one thread per connection (an actor).
  Dynamic typing resolved in `exec.rs` (see ADR-006). Its tests need nothing external.
  `remote.rs` opens a file on a server (ADR-018) by running that server's `sqlite3 -safe`
  through `ConnectParams::remote` (an SSH exec channel on the tunnel's last hop,
  `spelaion-ssh::Tunnel::exec`): it must never let a dot-command or an incomplete statement
  reach the shell. Catalog queries are shared in `catalog.rs`. Tests against
  `testing/docker/sqlitehost` with `SPELAION_TEST_SSH`.
- `crates/spelaion-driver-sqlserver`: `tiberius` over rustls, one actor task per connection
  (ADR-017). T-SQL batches (`GO`, `BEGIN … END`) split in `dialect.rs`; a batch may return
  several result sets. Cancel is a TDS Attention through a patched tiberius in
  `vendor/tiberius` (read its `PATCHED.md` before upgrading). Tests against the Docker SQL
  Server with `SPELAION_TEST_MSSQL=127.0.0.1:11433`.
- `crates/spelaion-driver-duckdb`: `duckdb` (bundled) in one thread per connection, one open
  per file shared by clones (ADR-016). Arrow from DuckDB converted in `exec.rs`; nested types
  written as DuckDB's text in `format.rs`. Its type zoo (`tests/duckdb.rs`) compares every
  type with `CAST(… AS VARCHAR)`; it needs nothing external.
- `crates/spelaion-config`: TOML files from `.spelaion/` and the personal directory,
  variables, secret references and repository trust (ADR-009; `trust.rs` pins each repository
  connection and tunnel as trusted, and the resolver refuses one that changed, ADR-022: any
  change there needs tests that try to get around it). If you change `model.rs`,
  regenerate the schema: `SPELAION_UPDATE_SCHEMA=1 cargo test -p spelaion-config`.
  `draft.rs` turns the add/edit connection form into a validated file (secrets go to the
  keychain, never to the file); editing touches only the changed keys with `toml_edit`.
  A connection's `description` is the user's free text (the explorer's tooltip, and what
  `list_connections` tells agents a database is for) and its `tags` are short labels the
  explorer filters connections by (`normalize_tags` in `draft.rs`): both bounded and without
  hidden characters (`load.rs`), and like every key part of what a trusted repository pins.
  `tunnel_draft.rs` does the same for SSH tunnels (hops matched by position; a stored hop
  password is forgotten when that hop points to another server). `dbeaver.rs` reads DBeaver's
  connections and saved credentials for the import (passwords only as `Secret`, to the keychain).
- `crates/spelaion-ssh`: tunnels with `russh` (ADR-007). It does not know the configuration:
  it receives a `TunnelSpec`. `KnownHosts` checks the team's `.spelaion/known_hosts` first
  (ADR-022; the resolver passes it only when trusted and unchanged), then the user's. Dialogs go through the `Interaction` trait. Tests against the
  Docker bastions with `SPELAION_TEST_SSH=127.0.0.1:2221`. A tunnel may go through a
  SOCKS5 or HTTP proxy (`proxy.rs`, ADR-027): its first hop, or the database itself with no
  hops; tests against `testing/docker/proxy` with `SPELAION_TEST_SOCKS=127.0.0.1:11080` and
  `SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128`.
  Kubernetes (ADR-015) is not a tunnel file but a connection's `[kubernetes]` section:
  `spelaion-core::kube` runs `kubectl port-forward` and lists contexts, namespaces, names and
  ports for the form's Kubernetes tab; tests against a kind cluster with
  `SPELAION_TEST_K8S=<kubeconfig>` (see `testing/k8s`).
- `crates/spelaion-core`: `open_connection` (resolved configuration → tunnel from the
  `TunnelPool` if any → driver → `Session`), `Session` (splits and runs scripts, stops at
  the first error) and `ResultStore` (results in the core, served in Arrow IPC windows).
  `catalog.rs` lists the engines of the "add connection" picker (planned ones included).
  `classify.rs` and `readonly.rs` are the read-only mode barriers (ADR-008): any change
  there needs tests that try to get around them. `guard.rs` lists the statements the app
  must confirm before running (DELETE/UPDATE without WHERE, DROP, TRUNCATE, production
  writes); `run_script` refuses unconfirmed risky scripts. `health.rs` keeps idle sessions
  alive and marks a session lost (ADR-028): every use goes through its `Tracked` connection, and
  nothing is ever reconnected or re-run there. `sort.rs` orders stored results
  (`results.rs` combines the sort, the text filter and the column filters of `filter.rs` into
  one view)
  and `cursor.rs` finds the statement under the editor's cursor. `table_data.rs` writes and checks
  the paged SELECT of the table data tab (the filter typed by the user cannot change it).
  `edit.rs` writes and applies the edits of a table data tab (ADR-014: primary key from the
  database, one transaction, one row per statement).
  `objects.rs` writes the explorer's object actions (ADR-026: qualified names, rename, drop,
  create, new index…); `tests/object_actions.rs` runs each engine's statements on that engine.
  `write.rs` holds the barriers and the transaction of agents' writes (ADR-019, ADR-020: one
  statement, never destructive, only the kinds on its allow list). Each engine's read barrier
  is in `readonly.rs` (ADR-020) and is tested in `tests/agents_engines.rs`; like
  `classify.rs`, any change there needs tests that try to get around it.
- `crates/spelaion-store`: local SQLite (audit, query history, saved queries, personal settings,
  the explorer's filters and bookmarks; later the schema cache). `spelaion-core::QueryHistory` owns its desktop actor and wraps
  session execution to record each executed statement before its completion is delivered.
- `crates/spelaion-mcp` + `apps/cli`: `spelaiondb mcp` and utilities (`connections`, `trust`,
  `audit`, `check`: the team repository's CI check, from `spelaion-config::check`, ADR-022). In `spelaiondb mcp`, stdout is the protocol: no `println!`. Reads and writes
  (`execute_write`, only with `[mcp] writes = "allow"`) use separate sessions.
- `apps/desktop`: Tauri 2 + React app. `src-tauri` is a thin adapter over the core; the
  command types are generated into `src/bindings.ts`
  (`cargo run -p spelaion-desktop --features bindings --bin export-bindings`; CI checks it is up to date).
  `fetch_window` is the only command outside tauri-specta (binary Arrow, cells already as
  text). Our own canvas grid (`ResultGrid.tsx`). Smoke test against the real app:
  `node apps/desktop/e2e/smoke.mjs` (after `npx tauri build --no-bundle`).
  Automatic updates (ADR-021): `src-tauri/src/update.rs` (Tauri's updater plugin, reading
  `latest.json` from the latest GitHub release, so releases are never pre-releases;
  `SPELAION_NO_UPDATES` switches them off, as the smoke test does). Update artifacts are signed only in the release workflow
  (`tauri.release.conf.json`, secret `TAURI_SIGNING_PRIVATE_KEY`); never commit a private key.
  Installers: `npx tauri build` (NSIS + MSI on Windows). Releases: pushing a tag `vX.Y.Z`
  runs `.github/workflows/release.yml` (installers for every OS, the `spelaiondb` CLI, notes from
  `docs/releases/vX.Y.Z.md` plus every commit since the previous tag, from
  `scripts/release-notes.sh`, so commit subjects must read well in a changelog); the tag must
  match the version in `Cargo.toml` and `tauri.conf.json`.

## Branches
Several people and agents work at the same time: nobody pushes to `dev` or `main`.
- The repository is public: `spelaion-dev/spelaiondb`. `dev` and `main` are protected: they
  only change through pull requests whose CI is green (the four jobs of `ci.yml`), and they
  cannot be force-pushed or deleted.
- **One task, one short branch**, named after who works on it and what (`pablo/filters`,
  `paco/issue-11`, `claude/…`, `codex/…`), from an up-to-date `dev`. Never work on someone
  else's branch.
- **Fetch before starting and before pushing** (`git fetch` and rebase on `origin/dev`):
  someone else may have merged in the meantime. Never force-push a branch someone else uses,
  and never rewrite `dev` or `main`.
- Still run the local checks below before each commit: CI confirms them, it does not replace
  them.
- **Pull requests target `dev`** (see `CONTRIBUTING.md`). Merge one when CI is green and it
  follows these rules, with "Rebase and merge" (one commit per change, as written), and delete
  its branch. GitHub's auto-merge can merge it as soon as CI passes.
- **Security files need a person:** a pull request that touches a file listed in
  `.github/CODEOWNERS` (the read-only and write barriers, secrets, trust, SSH host keys, the
  patched TLS libraries, updates, CI itself) waits for an owner's approval. The author cannot
  approve their own: the other owner does.
- An issue is the unit of work: assign it to yourself before starting, so two people do not
  change the same thing.
- To release, open a pull request `dev` → `main` and merge nothing else into `dev` until it is
  tagged (fixes for the release only). Merge it when green, with a merge commit; a maintainer
  then fast-forwards `dev` to `main` (`git merge --ff-only origin/main`, the one push to `dev`
  that is not a pull request), so they stay in step, and tags `main` with `vX.Y.Z`. Never turn
  on "delete branch on merge": it would delete `dev`.

## Non-negotiable rules
- `spelaion-core` does not depend on Tauri. The app, the CLI and the MCP are thin adapters.
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
  `SPELAION_TEST_PG=127.0.0.1:15432 SPELAION_TEST_MYSQL=127.0.0.1:13306
  SPELAION_TEST_MARIADB=127.0.0.1:13307 SPELAION_TEST_MSSQL=127.0.0.1:11433
  SPELAION_TEST_SSH=127.0.0.1:2221 SPELAION_TEST_SOCKS=127.0.0.1:11080
  SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128 cargo test --workspace`.
- `cargo deny check` if dependencies change.
- Frontend (`apps/desktop`): `npm run build` with no type errors and, if the UI changes,
  the smoke test `node e2e/smoke.mjs` (drives the real app over WebView2 CDP).
- If something could not be tested (for example Pageant or WebKitGTK), say so explicitly.

## Development environment
- Windows 11 is the main machine. Linux is tried with WSL (`scripts/setup-wsl.sh`) as a
  first look; definitive measurements on real Linux.
- Test databases and SSH bastions: `testing/docker` (see its README).
