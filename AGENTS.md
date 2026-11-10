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
- Everything in the repository is in English: code, comments, docs, UI text, tests and
  commit messages.

## Workspace map
- `crates/spelaion-driver-api`: driver contract (ADR-005). Engines are becoming plugins in
  their own processes, each in its own repository (ADR-041, ADR-042, after v0.9): anything
  added to this contract must be able to travel as a message (owned data; no closures, trait
  objects or streams in parameters), and new core code does not name a driver crate. Results
  in Arrow with
  `spelaion.logical` / `spelaion.native_type` metadata per column; `Value` for single cells.
  The explorer's folders (ADR-024): `list_objects` (relations), `list_schema_objects`,
  `list_server_objects` and `object_definition`, the last three in each driver's `objects.rs`
  (SQLite's in `catalog.rs`); each driver's test file checks them against its engine.
  `list_referencing_keys` is `describe_table`'s foreign keys seen from the other side: the keys
  of every table that point to one (in each driver's `introspect.rs`). `list_columns` is every
  column of a schema in one catalog query (metadata search, ADR-043), and `execute_bound` runs
  one read with text values bound to `bound_placeholder`'s placeholders (MySQL binds them to
  user variables with a prepared `SET`, then reads with the text protocol;
  `Capabilities::bound_params` is false for SQLite over SSH).
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
  password is forgotten when that hop points to another server).
  Importing connections from other clients (ADR-039): `import.rs` is the flow every client
  shares (the `ImportedConnection` model, drafts, duplicates found by name or by the database
  they reach, replace in place, target folder, what was not mapped, passwords read from another
  app's keychain only on import); a client is a reader that returns `ImportedConnection`s.
  `dbeaver.rs` reads DBeaver's connections and saved credentials (passwords only as `Secret`,
  to the keychain; its connection types map to the closest built-in type); `datagrip.rs` reads
  DataGrip's (and other JetBrains IDEs') XML data sources and SSH configurations, tested against
  files a real DataGrip wrote (`tests/fixtures/datagrip`); `tableplus.rs` reads TablePlus's own
  `Connections.plist` (never its encrypted `.tpmaster` store) and its exported file, encrypted in
  RNCryptor's format with the password the user typed (the export's passwords only as `Secret`;
  the SSH key it carries is never kept), tested against files a real TablePlus wrote
  (`tests/fixtures/tableplus`).
  `workbench.rs` reads MySQL Workbench 8.0's `connections.xml` (groups from `Group/Name`, SSH
  as a one-hop tunnel; passwords only from the macOS keychain on import, never its Windows DPAPI
  vault: the workspace forbids `unsafe`), tested against a file a real Workbench wrote
  (`tests/fixtures/workbench`).
  `pgadmin.rs` reads pgAdmin 4's exported servers JSON (no passwords in it: the preview says they
  are asked) or its own `pgadmin4.db`, opened read-only and never selecting its encrypted password
  columns; a database row is turned into the export's shape so one mapping serves both. Tested
  against files a real pgAdmin wrote (`tests/fixtures/pgadmin`).
  `beekeeper.rs` reads Beekeeper Studio's `app.db` read-only (connections, nested folders);
  its database and SSH passwords are decrypted the way Beekeeper does, with the install's
  `.key` (opened with the key built into Beekeeper), and only ever held as `Secret`s; other
  saved secrets are listed, never decrypted. Tested against tables a real Beekeeper wrote
  (`tests/fixtures/beekeeper`).
  ADR-030: `copy.rs` duplicates a connection next to the original (its keychain password copied
  inside the core, a team copy only from a trusted original) and reads pasted clipboard text as
  untrusted input (a personal connection through the form's checks, password always asked, no
  `${env:…}`, nothing that runs a program); `engine_change.rs` maps a connection to another
  engine (kept, changed, dropped, to fill in); `name_pattern.rs` renders the name of new
  connections from the personal pattern. Any change to `copy.rs` needs tests that try to get
  around it.
  `relocate.rs` moves a connection to another folder (its file, its keychain password, and the
  trust pin as it was, so a move never makes a changed or new repository connection usable);
  any change there needs tests that try to get around it (`tests/moves.rs`).
  `types.rs` resolves a connection's type (ADR-029): `env` names one by id (built-in `dev`,
  `test`, `prod`/`production`, or one in `.spelaion/connection-types.toml`, honoured only as
  trusted, or in the user's `connection-types.toml`); looks come from the user first, and
  safeguards only add up across layers, so nothing can loosen Production. Any change there
  needs tests that try to get around it (`tests/connection_types.rs`); `types_edit.rs` writes
  the user's types.
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
  `TunnelPool` if any → driver → `Session`), `Session` (splits and runs scripts: stop,
  continue or ask on an error, with a statement timeout and a summary; the "ask" pause and its
  ticket are in `script.rs`, ADR-035) and `ResultStore` (results in the core, served in Arrow
  IPC windows).
  `catalog.rs` lists the engines of the "add connection" picker (planned ones included).
  `classify.rs` and `readonly.rs` are the read-only mode barriers (ADR-008): any change
  there needs tests that try to get around them. `guard.rs` lists the statements the app
  must confirm before running (DELETE/UPDATE without WHERE, DROP, TRUNCATE, and what the
  connection's type asks for: data changes on Production, every run where it confirms
  execution, ADR-029); `check_script` refuses unconfirmed risky scripts, so the UI never
  decides it. `health.rs` keeps idle sessions alive, marks a session lost (ADR-028) and
  closes the ones a connection type says are idle too long, never with a transaction open
  (ADR-029): every use goes through its `Tracked` connection, and
  nothing is ever reconnected or re-run there. `transaction.rs` (ADR-031) is an editor tab's
  transaction: the state always comes from the driver (`Connection::transaction_state`, read
  after every statement), manual commit opens one with the engine's `BEGIN`, smart commit is a
  connection type safeguard, and `Session::close_needs_confirmation` is the check before
  closing a tab, disconnecting or quitting; core-written statements use `run_generated`, never
  the manual-commit `BEGIN`; `tests/transactions.rs` runs it on every engine. `sort.rs` orders stored results
  (`results.rs` combines the sort, the text filter and the column filters of `filter.rs` into
  one view; `find.rs` is the grid's Ctrl+F over that view, matches served in windows;
  `selection_stats.rs` is the calc panel's numbers over a block of that view, from its bounds
  only, stopped by a newer one or a cancel; `chart.rs` prepares a chart's points from that view,
  ADR-040: at most `MAX_CHART_POINTS`, rows combined by X, and it writes the saved PNG or SVG)
  and `cursor.rs` finds the statement under the editor's cursor. `progress.rs` thins a run's row
  counts to a few a second for the app (a big result streams hundreds of batches a second;
  every other event and the final count go through as they come). `table_data.rs` writes and checks
  the paged SELECT of the table data tab (the filter typed by the user cannot change it), and
  writes "filter by this value" conditions into its WHERE from the typed cell.
  `references.rs` follows foreign keys both ways from a row of a table data tab: the keys are
  read from the catalog again each time (the UI only names one), the values come from the
  stored result and the filter is `table_data::key_filter`'s; `tests/references.rs` follows
  composite, multi-level and self-referencing keys on every engine.
  `data_search.rs` (ADR-043) searches a value in the tables the user chose: one `SELECT` per
  table written from the catalog and the columns' types, the value always bound as a parameter
  (`Connection::execute_bound`), each a read in the engine's read-only transaction in a session
  of its own, bounded (rows per table, total hits) and cancellable; `tests/data_search.rs` runs
  it on every engine, 50 tables cancelled mid-way included. Metadata search is the app's
  (`dbSearch.ts` over `catalog.ts`, which keeps each schema's columns once read).
  `column_stats.rs` counts a table data tab column's statistics on the server: statements
  written from the catalog's names, the tab's filter checked like a page, each one a read in
  the engine's read-only transaction (the export barrier) in a session of its own, sampled past
  100,000 rows; `tests/column_stats.rs` runs it on every engine.
  Server observation (the process monitor's other views): `observe.rs` runs each statement
  only after `classify_read_only` accepts it, as session metadata; `locks.rs` reads each
  engine's locks and waits and builds the blocking tree (ending a blocker always goes through
  `processes.rs`'s `control_process`), `server_settings.rs` the settings, variables and status,
  `table_stats.rs` a table's sizes and row estimate from the catalog; `tests/server_observation.rs`
  makes one session wait for another's row lock on every engine and checks the pair.
  `export.rs` formats copies and exports (HTML, IN lists, column names); `tests/grid_engines.rs`
  runs those conditions, lists and names on every engine.
  `export.rs` also holds the CSV options and formula escaping of files;
  `export_stream.rs` streams a query, a table data tab or explorer tables to a file
  (ADR-034): only a single read is ever run again, in its own session and inside the engine's
  read-only transaction, one batch held at a time; like `readonly.rs`, any change there needs
  tests that try to get around it (`tests/export_stream.rs`).
  `edit.rs` writes and applies the edits of a table data tab (ADR-014: primary key from the
  database, one transaction, one row per statement).
  `value_view.rs` is the cell viewer's value, read from the stored result when the viewer opens
  (past `VIEW_LIMIT` only with "load anyway"): images recognised by their bytes (PNG, JPEG, GIF,
  WebP only, handed over as a `data:` URL; never SVG or HTML), XML pretty-printed as text, a value
  saved to a file byte for byte, and a file loaded into a cell as one `edit.rs` UPDATE by primary
  key; `tests/value_files.rs` runs it on every engine.
  `value_pick.rs` is the cell panel's value pickers (SDB-58): a date and time written with the
  column's precision as the grid shows it (UTC `+00` for a time zone; SQL Server's `datetime`
  and `smalldatetime` rounded as the server does), the allowed values of an enum, a `SET` or a
  `CHECK (… IN (…))` read from the catalog (SQLite's from its `CREATE TABLE` text) only when
  exactly one list can be read reliably, and a foreign key's search: a checked page of the
  referenced table (never all of it), its filter written like "filter by this value". A pick is
  only a cell edit's text, applied by `edit.rs`; `tests/value_pickers.rs` reads every pick back
  on every engine, and searches a million-row table.
  `import/` loads a file or pasted text into a table (ADR-038): `source.rs` reads CSV, TSV, JSON
  arrays and Markdown tables as untrusted input (bounded records, columns and whole reads),
  `detect.rs` detects types and converts the few values servers disagree on, and `mod.rs` writes
  the DDL, checks read-only and the guard, and loads the rows in transactions through
  `Connection::insert_rows`, which binds every value (`COPY` on PostgreSQL, prepared inserts on
  MySQL, a TDS bulk load on SQL Server, the appender on DuckDB): never SQL text.
  `tests/import.rs` loads a million rows on every engine; any change there needs tests that try
  to get around it.
  `objects.rs` writes the explorer's object actions (ADR-026: qualified names, rename, drop,
  create, new index…); `tests/object_actions.rs` runs each engine's statements on that engine.
  `conn_test.rs` is the connection test (ADR-032): one step at a time (resolve, proxy, each
  hop through `Tunnel::connect_observed`, Kubernetes, reaching the target with
  `Tunnel::probe`, login), its own tunnel, Stop that closes what it opened, failures classified
  there and a `Redactor` every label, message and the diagnostics text pass through: any change
  there needs tests that try to leak a secret (`tests/conn_test.rs`). A connection's
  `connect_timeout` is the database's (`ConnectParams::connect_timeout`, held whole by
  `open.rs`); a tunnel file's is per SSH hop.
  `save_name.rs` keeps a save dialog's file name in step with the chosen file type (Linux's
  GTK dialogs, `apps/desktop/src-tauri/src/file_dialog.rs`).
  `sql_file.rs` reads and writes the editor's `.sql` files (ADR-036: UTF-8 with or without a
  BOM, UTF-16, Windows-1252, the file's line ending, temp file + rename through
  `spelaion-config::atomic_file`, which keeps the mode and follows links, a content hash that
  refuses to overwrite a file changed on disk); `open_tabs.rs` keeps the open tabs (`TabKeeper`
  writes them at most a second after a change) and checks them before they come back
  (`restore_tabs`): never results, secrets or session state, and nothing is run.
  `session_setup.rs` (ADR-037) applies a connection's `[session]` table every time a session
  opens (isolation level, default schema, setup statements), from `open_connection`; on a
  read-only connection, a Production type and sessions that only read
  (`OpenOptions::read_only_session`), every setup statement must first pass
  `classify_session_setup` in `classify.rs`: like `classify.rs`, any change there needs tests
  that try to get around it (`tests/session_setup.rs`).
  `write.rs` holds the barriers and the transaction of agents' writes (ADR-019, ADR-020: one
  statement, never destructive, only the kinds on its allow list). Each engine's read barrier
  is in `readonly.rs` (ADR-020) and is tested in `tests/agents_engines.rs`; like
  `classify.rs`, any change there needs tests that try to get around it.
- `crates/spelaion-store`: local SQLite (audit, query history, saved queries, personal settings,
  the explorer's filters and bookmarks, the editor's open tabs per workspace and recent files in
  `tabs.rs`; later the schema cache). `spelaion-core::QueryHistory` owns its desktop actor and wraps
  session execution to record each executed statement before its completion is delivered.
- `crates/spelaion-mcp` + `apps/cli`: `spelaiondb mcp` and utilities (`connections`, `trust`,
  `audit`, `check`: the team repository's CI check, from `spelaion-config::check`, ADR-022). In `spelaiondb mcp`, stdout is the protocol: no `println!`. Reads and writes
  (`execute_write`, only with `[mcp] writes = "allow"`) use separate sessions. What agents may
  do on a connection is `spelaion-core::agent_access` (`write.rs`): the user's read-only mode
  for every agent (ADR-033, `mcp.read_only` in the store, read on every call, failing closed)
  only takes writes away. Query rows go out as JSON or TOON (`format.rs`); any change there
  keeps every cell quoted so data cannot add rows or fields.
  `spelaion-config::mcp_clients` finds each AI client's file and writes our entry there for the
  app's MCP page (ADR-033): only our entry changes, JSON edited as text at the spans it finds,
  Codex's TOML with `toml_edit`, a backup first, and only the change the user saw (its
  fingerprint); the new file and the backup keep the old file's mode (owner-only when new) and
  a symbolic link stays a link. Tests and the smoke test point it at
  `SPELAION_MCP_CLIENTS_HOME`, never at the user's files; `SPELAION_CLI` names the `spelaiondb` it installs. The installers ship the CLI
  next to the app (`externalBin`, only in `tauri.release.conf.json`).
  Every MCP tool call goes to the store's append-only `audit` table with its tool and its
  process's session; the app's Activity tab reads it through `spelaion-core::audit`, which masks
  secrets (`redact_secrets`) before anything is shown or exported. Clearing (`clear_audit`) is
  the only delete and leaves a record.
- `apps/desktop`: Tauri 2 + React app. `src-tauri` is a thin adapter over the core; the
  command types are generated into `src/bindings.ts`
  (`cargo run -p spelaion-desktop --features bindings --bin export-bindings`; CI checks it is up to date).
  `fetch_window` is the only command outside tauri-specta (binary Arrow, cells already as
  text). Our own canvas grid (`ResultGrid.tsx`). App commands (name, shortcut, menus) are
  registered once in `src/commandRegistry.ts` and run through `src/commands.ts`: the tab and
  file menus, the activity bar, the global shortcuts and the command palette
  (`CommandPalette.tsx`, Ctrl+Shift+P) all read it, and `scripts/commands.test.mjs` checks the
  menus against it. The palette's objects are only what the explorer already read (`catalog.ts`). Smoke test against the real app:
  `node apps/desktop/e2e/smoke.mjs` (after building the app and the CLI in one command, so
  bundled DuckDB is compiled once: `npx tauri build --no-bundle -- -p spelaion-desktop -p
  spelaion-cli`; a release build on 16 GB needs `CARGO_BUILD_JOBS=3`, see `CONTRIBUTING.md`).
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
  `paco/sdb-11`, `claude/…`, `codex/…`), from an up-to-date `dev`. Never work on someone
  else's branch.
- **Fetch before starting and before pushing** (`git fetch` and rebase on `origin/dev`):
  someone else may have merged in the meantime. Never force-push a branch someone else uses,
  and never rewrite `dev` or `main`.
- Still run the local checks below before each commit: CI confirms them, it does not replace
  them.
- On a pull request that changes nothing Rust reads (only documentation, or only the frontend
  outside `src-tauri` and `bindings.ts`), CI's two Rust jobs pass without running; the push to
  `dev` after the merge runs them. `.github/workflows/needs-rust.sh` has the list: if Rust
  starts reading a new place outside its crates, name it there in the same pull request.
- **Pull requests target `dev`** (see `CONTRIBUTING.md`). Merge one when CI is green and it
  follows these rules, with "Rebase and merge" (one commit per change, as written), and delete
  its branch. GitHub's auto-merge can merge it as soon as CI passes.
- **Security files need a person:** a pull request that touches a file listed in
  `.github/CODEOWNERS` (the read-only and write barriers, secrets, trust, SSH host keys, the
  patched TLS libraries, updates, CI itself) waits for an owner's approval. The author cannot
  approve their own: the other owner does.
- A Plane work item is the unit of work (see "Work tracking" below).
- To release, open a pull request `dev` → `main` and merge nothing else into `dev` until it is
  tagged (fixes for the release only). Merge it when green, with a merge commit; a maintainer
  then fast-forwards `dev` to `main` (`git merge --ff-only origin/main`, the one push to `dev`
  that is not a pull request), so they stay in step, and tags `main` with `vX.Y.Z`. Never turn
  on "delete branch on merge": it would delete `dev`.

## Work tracking
The team's tracker is **Plane**: workspace `spelaion`, project `spelaiondb`, identifier `SDB`
(agents reach it through the `plane` MCP server). GitHub issues are for reports from people
outside the team.
- Each release is a Plane module (`v0.7 — …`). Labels give the type, the area and the size
  (S / M / L); `adr-needed` marks a change of direction.
- **Before starting:** take an item, assign it to yourself and move it to *In Progress*, so
  two people do not change the same thing. Read its description: its "Constraints" and "Done
  when" are part of the task.
- **Opening the pull request:** move the item to *In Review*, link the pull request from the
  item, and write the item's identifier (`SDB-12`) in the pull request's description. Keep it
  out of commit subjects: they go into the release notes as they are.
- **Merged:** move the item to *Done*. Dropped: *Cancelled*, with a comment saying why.
- Work found along the way becomes a new item in *Backlog*, not part of the current pull
  request.
- "Decide: …" items end in an ADR. ADRs live in `docs/adr/`, never only in Plane: the item
  links to the ADR's pull request, and any work that follows gets its own items.
- An external GitHub issue that we accept gets a Plane item that links to it. When its fix is
  merged into `dev`, close the issue by hand ("Fixed by #N"): closing keywords only act when
  the change reaches `main`.

## Non-negotiable rules
- `spelaion-core` does not depend on Tauri. The app, the CLI and the MCP are thin adapters.
- All security lives in the core: statement splitting and classification, permissions,
  audit and secrets. Never in React or in a Tauri command.
- Secrets are never written to logs, config files, history or MCP responses.
- After reconnecting a tunnel or a connection, nothing the user ran is ever re-run
  automatically. Only the connection's session setup runs, as part of opening every new session
  (ADR-037).
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
- Read `docs/ui-guidelines.md` before any UI change: use its tokens (never literal colors,
  sizes, radii or shadows), its components, its places for actions and its shortcut map (a new
  shortcut goes into it), its wording for dialogs and errors, and its verification steps.

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
