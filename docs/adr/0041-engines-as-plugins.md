# ADR-041: Engines as plugins: every driver in its own process, behind the same contract

- **Status:** Accepted (stage 0 implemented, 2026-11-13: see "Implementation"; the engines
  leave the core as their repositories are created. Where our plugins are built and how they
  are checked is superseded by ADR-042; how our plugins are found, installed and updated, by
  ADR-055)
- **Date:** 2026-11-10
- **Supersedes:** the "Extensibility" part of ADR-005 (the rest of ADR-005 stands)
- **Builds on:** ADR-002 (security lives in the core), ADR-004 (no daemon), ADR-005 (driver
  contract), ADR-018 (SQLite on a server), ADR-019 and ADR-020 (agents and each engine's
  barriers), ADR-021 (signed updates), ADR-022 (trust), ADR-028 (lost sessions)

## Context
SpelaionDB has five engines, each a Rust crate compiled into the core. ADR-005 left "drivers in
external processes" for later and asked for a contract "designed so it can travel as messages".
Three things bring later forward:

- **Breadth.** Every engine we add today grows the core, the app and every build. Tabularis
  ships PostgreSQL, MySQL and SQLite and reaches the rest through plugins (29 published when
  this was written, DuckDB, SQL Server and Oracle among them), each a separate program that
  talks JSON-RPC over stdin and stdout; its README even calls its built-in PostgreSQL driver
  deprecated in favour of the plugin. Some engines (Oracle, Db2) are reached in practice
  through their vendor's client library or a JDBC driver, which we cannot, or would rather
  not, link into the core.
- **Weight.** DuckDB is compiled from C++ inside the core: the heaviest single dependency of a
  build from scratch (not measured per engine yet: stage 1 does), and every test executable of
  `spelaion-core` links it. Nobody who only uses PostgreSQL needs it in the process.
- **One path.** Keeping the five engines inside and adding the others as plugins would make
  two classes of engine for good: the ones with everything, and the ones with what the plugin
  protocol happens to carry. A protocol that PostgreSQL does not have to pass through will be a
  lowest common denominator, which is what ADR-005 exists to avoid.

What must not change: the core decides what runs (splitting, classification, read-only
barriers, the guard), holds the secrets and the tunnels, and audits (ADR-002, AGENTS.md); each
type's text is the server's own; nothing is re-run after a reconnection.

## Decision
**Every engine becomes a plugin: a program of its own that the core starts and talks to. The
five engines we have are migrated one at a time, ship inside the installers, and behave as they
do today. New engines are only written as plugins.**

1. **A plugin is the wire, not the rules.** A plugin connects, runs statements, cancels,
   introspects and loads rows: the `Driver` and `Connection` contract of `spelaion-driver-api`,
   as messages. The core keeps a `PluginDriver` that implements those same traits by talking to
   the process, so the session, the result store, the UI and the MCP do not know the difference
   (ADR-005: they follow capabilities, never the engine's name).
   What decides whether something may run stays compiled into the core and is never asked of a
   plugin: each engine's **dialect** (statement splitting, quoting, classification), its
   read-only and agents' barriers (`classify.rs`, `readonly.rs`, `write.rs`, `guard.rs`) and
   the SQL the core writes (`table_data.rs`, `objects.rs`, `edit.rs`…). They move out of the
   driver crates into the core's own code, which is small and has no network or native
   dependency. The core stops depending on the driver crates.
2. **The protocol.** Over the plugin's stdin and stdout, in frames. Control messages are
   JSON-RPC 2.0 (`initialize`, `connect`, `execute`, `cancel`, the introspection calls,
   `insert_rows`, `close`). **Rows never travel as JSON:** result batches, and the rows of an
   import, are Arrow IPC in binary frames, with the same field metadata as in process
   (`spelaion.logical`, `spelaion.native_type`). `execute` answers with the same events in the
   same order as `ExecEvent` (columns, batches, notices, done). `cancel` can arrive while a
   statement runs and must be obeyed then. `initialize` exchanges the protocol version and the
   plugin's capabilities. The written specification is `docs/plugin-protocol.md`, part of
   stage 0; it is **experimental until 1.0** and may change in any 0.x release.
3. **One process per engine, per program.** The app, and `spelaiondb mcp` on its own, each
   start a plugin the first time a connection of that engine opens and reuse it for every
   session of that engine (a session id in each message). DuckDB needs it: one open per file
   and process (ADR-016). The plugin exits when its stdin closes, and the core kills it when
   the program ends: no process outlives the app, which is what ADR-004 avoids. A plugin that
   dies marks its sessions lost, as a dropped connection does (ADR-028): nothing is re-run,
   and the next connection starts it again.
4. **Secrets and the network stay in the core.** The core resolves the password from the
   keychain and sends it in the `connect` message, over stdin only: never in the arguments,
   the environment or a file. What a plugin writes to stderr goes to the log through the
   redactor. Tunnels, proxies and Kubernetes forwards are opened by the core as today; the
   plugin gets `connect_addr` (the local end) and the real host name for TLS, and does its own
   TLS. A plugin never reads configuration files, the keychain, the local store or another
   plugin.
5. **Whose plugin it is decides what it may do.**
   - **Ours** (built from this repository by the release workflow): shipped inside the
     installers and next to the CLI, the way the CLI ships with the app (ADR-033), and always
     the same build as the core: a plugin from another build is refused, so there is no
     compatibility table. Engines beyond the five may be downloaded the first time they are
     used instead of shipped, from our GitHub release, checked against the update key
     (ADR-021) and refused if the signature does not match. An engine of ours has its dialect
     and barriers in the core and its tests in CI, so read-only mode and agents work on it as
     ADR-019 and ADR-020 say.
     (Superseded by ADR-042 in where they are built and how they are checked: each plugin of
     ours has its own repository, a protocol version replaces "the same build", and what the
     app ships or downloads is pinned by hash in this repository, not checked against the
     update key.)
   - **Someone else's:** installed only by the user, from a file, after a dialog that says
     what it is (a program that runs with the user's permissions and receives the passwords of
     its connections), and pinned by its hash: a changed file is refused until trusted again
     (as ADR-022 pins repository files). Nothing in a repository's `.spelaion/` can install,
     name or point to a plugin. Its manifest names one of the core's dialects, or none.
     **Agents get nothing on it and read-only mode is not offered** (not a weaker one): the
     barrier ADR-020 asks of each engine is not ours to vouch for. No registry or marketplace.
     (Amended by ADR-055: the app has a Plugins page fed by a signed catalogue of our own
     plugins. Other people's are never in it and are still added only by the user, from a
     file or from an address typed into that page; the app says they are not ours to support.)
6. **SQLite on a server stays in the core** (ADR-018). It talks to the server's own `sqlite3`
   through a channel of the core's tunnel, which cannot be handed to another process, and it
   does not use the SQLite library. It is the one driver that is not a plugin.
7. **Stages, with the same gate for every engine.** A built-in driver is removed in the
   release where its plugin passes the gate, and not before: the two never ship together for
   one engine.
   - **Stage 0:** the protocol and its specification, `PluginDriver`, the plugin side as a
     library (so a Rust plugin implements the traits and gets the protocol for free), and the
     dialects and barriers moved into the core.
   - **Stage 1:** DuckDB, the heaviest and the one that tests one-process-per-engine.
   - **Stage 2:** SQL Server, MySQL and MariaDB, PostgreSQL, then SQLite files on this machine
     (last: the core links SQLite anyway for its store, ADR-011, so that one is only for
     having one path).
   - **Stage 3:** new engines of ours (ClickHouse and Oracle first: the most asked for when
     this was written).
   - **Stage 4:** other people's plugins, with the rules of point 5.
   - **The gate:** (a) the engine's whole test set passes through the plugin, unchanged: its
     type zoo, the core's tests that run on every engine, and its barrier tests
     (`tests/agents_engines.rs`, `tests/export_stream.rs`, `tests/session_setup.rs`); (b)
     streaming a million-row result and importing a million rows (`tests/import.rs`) take at
     most 15 % longer than in process, measured in the pull request; (c) the connection test
     still names the step that failed (ADR-032).
8. **When.** After v0.9, as a release of its own, before 1.0: the protocol is a contract other
   people will build on, and 0.x is where it can still change. v0.9's features are written
   against the built-in engines and are not held back.
9. **From today:** anything added to `spelaion-driver-api` must be able to travel as a message
   (owned data; no closures, trait objects or streams in parameters; `Remote` is the recorded
   exception), and new core code does not name a driver crate. Both keep the migration from
   growing while v0.9 is written.

## Consequences
- The core and the app no longer compile or link any engine: a build from scratch loses DuckDB,
  and the core's test executables get much lighter. **The total work goes up, not down:** each
  plugin is one more executable per platform, with its own copy of the async runtime, TLS and
  Arrow. What improves is that the work is split, runs in parallel and is redone only for the
  plugin that changed. The installers grow for the same reason; downloading the less common
  engines on first use (point 5) is what keeps that in check.
- Five working drivers are migrated with no feature to show for it. The gate makes that a
  matter of effort and not of risk: the tests already compare every type with the server.
- A plugin crash loses every session of that engine in that program, not only one tab.
- The core's engine tests need the plugin executables built first; `spelaiondb mcp` and the
  CLI archive must carry them; the release builds them for every platform.
- Each plugin is one more executable to code-sign when the app is (ADR-012); until then, one
  more unsigned executable for antivirus software to look at.
- A plugin can be written in any language, so the JDBC bridge ADR-005 mentions is a plugin
  like any other, and it may link what our licence does not let the core link.
- The project's "we ship our own drivers, no JDBC-style driver manager" note changes meaning:
  we still write and ship our own; they stop being inside the binary.

## Implementation (2026-11-13)
Stage 0 is in the repository; the protocol's specification is `docs/plugin-protocol.md`.

- **The protocol** (version 1.0, experimental until SpelaionDB 1.0): frames over the plugin's
  standard input and output, JSON-RPC 2.0 for control and Arrow IPC for rows, never JSON. The
  core sets the pace of a result with credit, so a large result does not fill its memory. Three
  crates: `spelaion-plugin-protocol` (the wire), `spelaion-plugin-sdk` (a plugin implements
  `Driver` and `Connection` and calls `run`) and `spelaion-plugin-host` (`PluginDriver` is a
  `Driver`: the session, the result store, the app and the MCP server see no difference).
- **What running every engine through it added**, before anything depended on it:
  `create_database` (a database that is a file is created by its driver); a `close` that stops a
  `connect` still in progress (a login the user stopped is not left open); SQLite on a server
  stays with the driver compiled in when the local one is a plugin; and the conformance set
  cancels a statement whose `execute` has not answered (MySQL answers only when its first
  response arrives).
- **Each pipe has a thread of its own, with blocking reads and writes,** on both sides: a
  plugin's process belongs to no caller's runtime, and a frame is one read of the pipe.
- **The rows of an import travel as columns of text** (`TextRows`, Arrow): the contract's
  `insert_rows` takes them, and a driver reads them in place.
- **The gate, measured** (Windows, optimised builds, the same build both ways, median of five,
  `scripts/measure-plugins.sh`): a million-row result takes 5.7 % longer through DuckDB's
  plugin; importing a million rows takes at most 13.9 % longer (DuckDB, into a new table;
  SQLite 13.7 %, PostgreSQL 11.2 %, MySQL 4.2 %, SQL Server 2.0 %, MariaDB 1.2 %). Before the
  rows travelled as columns and the pipes had their threads, the import took 47 % to 71 %
  longer on DuckDB. The fastest engines load a million rows in about a second, so 15 % is
  150 ms: expect a few points between two measurements.
- **The whole test suite passes through plugins, unchanged**: no test names a driver crate
  (they take it from the core's engines), and `scripts/test-through-plugins.sh` runs any
  `cargo test` with every engine through its plugin.
- **What an engine's leaving saves** (Windows, debug, `cargo test --workspace --no-run` from an
  empty target folder, each measured once, on the branches where the drivers are already
  gone): with every driver compiled in, 619 s, 7.8 GB of test executables and a 25.3 GB target
  folder; without DuckDB, 297 s, 2.2 GB and 10.0 GB; without DuckDB, SQL Server, MySQL/MariaDB
  and PostgreSQL, 218 s, 1.4 GB and 7.4 GB. DuckDB is most of it, as the Context expected.

## Alternatives
- **Hybrid for good** (the five inside, the rest as plugins): the least work, and two classes
  of engine with a protocol nothing demanding ever tests. Kept only as the order of the stages.
- **Tabularis's protocol, to run its plugins.** As far as its plugin guide shows (read on
  2026-11-10), results come back as pages of JSON rows, with binary values as text: that is
  the lowest common denominator ADR-005 rules out, and our type zoos would not pass through
  it. A plugin that hosts Tabularis plugins could be written later, under the rules for other
  people's plugins.
- **Dynamic libraries loaded into the process:** no isolation, an ABI to keep stable across
  compilers, and a crash or memory error in a driver is one in the core.
- **WASM:** ruled out in ADR-005 (sockets, TLS and async in WASI), and still so.
- **One process per connection:** better isolation, but DuckDB cannot share a file between
  processes, and every tab would pay a process start.
- **Plugins that bring their own dialect and barriers:** the core would be asking the code it
  is guarding against whether a statement is a read.

## Revisit if
- An engine cannot pass the gate's 15 % (large results or imports): that engine stays built in
  and this ADR records it, as it does for SQLite on a server.
- Passing a tunnel's channel to a plugin becomes possible and simple: SQLite on a server follows
  the others.
- Other people's plugins are what users ask for most: a signed registry, and whether an
  engine-enforced barrier declared by a plugin can ever be trusted, get their own ADR.
