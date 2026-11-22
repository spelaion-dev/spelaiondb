# SpelaionDB — rules for agents

An open-source (Apache-2.0) desktop database client, with first-class SSH tunnels and an
MCP server. **All the code is written by an agent and nobody reviews it line by line**:
the compiler, the tests and CI are the only reviewer. Write accordingly.

## Before touching anything
- Read `docs/architecture.md`. Use `docs/adr/README.md` to find and read the decisions governing the affected behavior. ADRs rule: if a change
  contradicts them, do not make it; propose a new ADR that supersedes the old one.
- Current state: the version in the workspace's `Cargo.toml`; what each release added is in
  `docs/releases/` (and the Plane modules). This file does not repeat it, so it cannot go stale.
- Everything in the repository is in English: code, comments, docs, UI text, tests and
  commit messages.

## Working in a component
Before editing, search `docs/agent-map/` for the component, file, or behavior
(`rg -in -C 3 'search term' docs/agent-map`). Read the matching guidance,
including cross-component contracts, required tests, and the ADRs it cites.
Keep new component-specific guidance in those files.

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
- **Pull requests target `dev`** (see `CONTRIBUTING.md`) and **merge through `dev`'s merge
  queue**, never straight: CI on a pull request only proves it against the `dev` it was opened
  on. Once it is green and follows these rules, put it in the queue: `gh pr merge --auto <N>`
  (it waits for the required checks, then queues; no merge method is given, the queue's is
  "Rebase and merge", one commit per change, as written). The queue runs CI again on top of the
  current `dev` and of the pull requests queued before it (`merge_group`) and merges only if
  that run is green too; one that fails there leaves the queue unmerged: rebase on `origin/dev`,
  fix it and queue it again. Delete its branch once it has landed. `gh pr merge --admin`
  bypasses the queue: only a maintainer, and only for what cannot wait for it.
- **Security files need a person:** a pull request that touches a file listed in
  `.github/CODEOWNERS` (execution and import barriers, secrets and redaction, trust and
  connection safeguards, SSH and remote SQLite boundaries, MCP client configuration, patched
  TLS libraries, updates and CI) waits for an owner's approval. The author cannot
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
- A lifecycle is one enum owned by the core, with its transitions in one place: no flags or
  `Option`s that together mean a state, no status as text; jobs share one cancel and one ending
  (ADR-060).

## Frontend style
- React + strict TypeScript. Types that cross from Rust are generated, not handwritten.
- Result data never goes into React state: it lives in the core and is requested in windows.
- A state the core has is read as its generated type, never rebuilt from events or flags; one
  that exists only in the page is a discriminated union whose `switch` ends in a `never` check
  (ADR-060).
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
  SPELAION_TEST_CLICKHOUSE=127.0.0.1:18123 SPELAION_TEST_ORACLE=127.0.0.1:11521
  SPELAION_TEST_LIBSQL=127.0.0.1:18095 SPELAION_TEST_NEO4J=127.0.0.1:17474
  SPELAION_TEST_DATABEND=127.0.0.1:18320 SPELAION_TEST_SPANNER=127.0.0.1:19021
  SPELAION_TEST_GIZMOSQL=127.0.0.1:31337
  SPELAION_TEST_SSH=127.0.0.1:2221 SPELAION_TEST_SOCKS=127.0.0.1:11080
  SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128 SPELAION_TEST_MONGO=127.0.0.1:27017
  SPELAION_TEST_PGBOUNCER=127.0.0.1:16432
  SPELAION_TEST_REDIS=127.0.0.1:16379 SPELAION_TEST_CASSANDRA=127.0.0.1:19142
  SPELAION_TEST_TDENGINE=127.0.0.1:16141
  cargo test --workspace`.
- Hive, Trino, Presto and InfluxDB run in a separate memory-bounded phase: on an isolated test Compose
  project, stop the first phase, start `hive trino presto influxdb bastion1 bastion2`, then run
  `SPELAION_TEST_HIVE=127.0.0.1:11010 SPELAION_TEST_TRINO=127.0.0.1:18080
  SPELAION_TEST_PRESTO=127.0.0.1:18081 SPELAION_TEST_INFLUXDB=127.0.0.1:18181 SPELAION_TEST_SSH=127.0.0.1:2221
  cargo test -p spelaion-core --test hive --test trino --test influxdb`. See `testing/docker/README.md`;
  the first workspace pass alone does not count as testing these engines.
- `cargo deny check` if dependencies change.
- Frontend (`apps/desktop`): `npm run build` with no type errors and, if the UI changes,
  the smoke test: `node e2e/smoke.mjs` on Windows (drives the real app over WebView2 CDP; build
  it with `--features spelaion-desktop/e2e`, see `CONTRIBUTING.md`),
  `node e2e/smoke-linux.mjs` on Linux (a shorter pass through WebDriver, see `CONTRIBUTING.md`).
  CI runs both on every pull request and push to `dev`, but neither is a required check.
  `smoke-windows.yml` runs `smoke.mjs` only up to the "SQL files" step, because GitHub's Windows
  runners have no `testing/docker`. That covers start, trust, SQLite, the grid, the editor, tabs,
  the explorer, table data, charts and SQL files. Every step from "direct PostgreSQL" to the end
  still has to be run by hand on Windows with `testing/docker` up: the server engines, tunnels,
  imports, DuckDB, the large results, restarts, the MCP page and quitting.
- If something could not be tested (for example Pageant or WebKitGTK), say so explicitly.

## Development environment
- Windows 11 is the main machine. Linux is tried with WSL (`scripts/setup-wsl.sh`) as a
  first look; definitive measurements on real Linux.
- Test databases and SSH bastions: `testing/docker` (see its README).
