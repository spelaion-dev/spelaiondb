# SpelaionDB — rules for agents

An open-source (Apache-2.0) desktop database client, with first-class SSH tunnels and an
MCP server. **All the code is written by an agent and nobody reviews it line by line**:
the compiler, the tests and CI are the only reviewer. Write accordingly.

## Before touching anything
- Read `docs/architecture.md` and the ADRs in `docs/adr/`. ADRs rule: if a change
  contradicts them, do not make it; propose a new ADR that supersedes the old one.
- Current state: the version in the workspace's `Cargo.toml`; what each release added is in
  `docs/releases/` (and the Plane modules). This file does not repeat it, so it cannot go stale.
- Everything in the repository is in English: code, comments, docs, UI text, tests and
  commit messages.

## Workspace map
- `crates/spelaion-driver-api`: driver contract (ADR-005). Engines are becoming plugins in
  their own processes, each in its own repository (ADR-041, ADR-042, after v0.9): anything
  added to this contract must be able to travel as a message (owned data; no closures, trait
  objects or streams in parameters), and `spelaion-core/src` names a driver crate only in
  `open.rs`, where `Engines` is built (its unit tests take a driver from `open::test_driver`).
  **No test names a driver crate either**: the core's tests take an engine's driver from
  `crates/spelaion-core/tests/engines/mod.rs` (`engines::postgres()`, `engines::create_database`),
  which is `Engines::builtin()`, and the tests of `spelaion-ssh` and `apps/cli` ask the core the
  same way, so an engine's tests run unchanged through its plugin (ADR-041's gate).
  A database that is a file (SQLite, DuckDB) is created by its driver (`Driver::create_database`,
  `Engines::create_file_database`), never by calling a driver crate.
  Results in Arrow with
  `spelaion.logical` / `spelaion.native_type` metadata per column; `Value` for single cells.
  The explorer's folders (ADR-024): `list_objects` (relations), `list_schema_objects`,
  `list_server_objects` and `object_definition`, the last three in each driver's `objects.rs`
  (SQLite's in `spelaion-sqlite-schema`'s `catalog.rs`); each driver's test file checks them against its engine.
  `list_referencing_keys` is `describe_table`'s foreign keys seen from the other side: the keys
  of every table that point to one (in each driver's `introspect.rs`). `list_columns` is every
  column of a schema in one catalog query (metadata search, ADR-043), and `execute_bound` runs
  one read with text values bound to `bound_placeholder`'s placeholders (MySQL binds them to
  user variables with a prepared `SET`, then reads with the text protocol;
  `Capabilities::bound_params` is false for SQLite over SSH).
  `Dialect::code_ranges` is what lies outside a script's strings, quoted names and comments, as
  that engine's splitter reads them (each dialect builds it from its splitter's own pieces with
  `code_ranges`): the editor's `:name` placeholders (`spelaion-core`'s `params.rs`) are looked
  for only there.
  `thread_cancel` is the cancel of the engines that run on a thread of their own (DuckDB, local
  SQLite): their interrupt only reaches the statement running at that moment, so a cancel is
  remembered for everything sent before it (a queued statement never starts, a running one is
  interrupted again until it ends) and never reaches what is sent after it. Every cancel test
  is bounded with `tokio::time::timeout`, so a lost cancel fails in seconds.
- `crates/spelaion-dialect`: each engine's `Dialect` (statement splitting, quoting, code
  ranges), one file per engine, with their tests. Only text: no network, no native library.
  They are the core's rules, not the driver's (ADR-041): the core takes an engine's dialect
  from `for_engine` (`spelaion-core`'s `dialect_of`); a driver has none to give, and
  uses its engine's file only for what it writes itself (the names in its catalog queries,
  the words it must recognise in a statement it sends). A new engine's dialect is added here,
  with its name in `for_engine`. MySQL's splitter (`mysql.rs`) counts a statement written only
  as executable comments (how `mysqldump` writes views and triggers) as a statement, and reads
  quotes as the session's `sql_mode` does (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`):
  `Dialect::with_sql_mode` gives the session's dialect, which `spelaion-core`'s `sql_mode.rs`
  reads when the session opens and after a statement that names `sql_mode`. T-SQL batches
  (`GO`, `BEGIN … END`) are split in `sqlserver.rs`; a `DECLARE` keeps the rest of its batch,
  where its variables live (the guard still checks each statement in it). SQLite's
  `skip_quote_or_comment` and `code_line_starts` are what `remote.rs` relies on to keep a
  dot-command away from the server's shell: any change to them needs its tests there too.
- The plugin protocol (ADR-041, `docs/plugin-protocol.md`: the specification, which the code
  must match; experimental until 1.0). An engine plugin is a program the core starts and
  talks to over its standard input and output: JSON-RPC 2.0 messages for control, Arrow IPC
  for rows (never JSON), in frames. Three crates:
  - `crates/spelaion-plugin-protocol`: the wire only. `frame.rs` (frames, at most 64 MiB,
    refused before anything is allocated), `message.rs` (every method's parameters and the
    driver's error) and `ipc.rs` (`BatchDecoder` reads one IPC message at a time because Arrow's
    own `StreamDecoder` does not see a schema until a batch follows it). A change that breaks
    is a new major `VERSION`; within one, changes only add.
  - `crates/spelaion-plugin-sdk`: the plugin's side. A Rust plugin implements `Driver` and
    `Connection` and calls `run`. It sends a batch only with credit from the core, reports a
    connection known to be closed, answers even when a driver panics and ends the program when
    its input closes: first its requests stop, its sessions are dropped and each driver's
    `Driver::shutdown` closes what it keeps open (DuckDB's shared files write what they hold),
    so a file is never left half written by a plugin that was only asked to end (SDB-360).
    Standard output is the protocol: a plugin never prints to it.
  - `crates/spelaion-plugin-host`: the core's side. `Plugin` is one process per program,
    started on the first connection and shared by every session of its engines. Its pipes are
    read and written on threads of their own with blocking calls, as the SDK's are (never
    `tokio::spawn` or tokio's own pipes there: the runtime that connected first may end before
    the others, and each read would be a trip through the runtime's blocking pool); `PluginDriver`
    and its connection are the `Driver` and `Connection` the core already uses. A plugin that
    stops loses its sessions (`is_closed`, ADR-028) and nothing is re-run. Stopping one
    (`Plugin::stop`, or the last of it dropped) closes its input and kills it only if it has
    not ended after `STOP_GRACE` (5 s): a plugin killed at once could leave a file half written. The password goes
    only in `connect`, and every error, notice and log line of a plugin has the secrets it was
    given taken out. `conformance.rs` is the set a plugin runs against itself in its own CI
    (ADR-042).
  `crates/spelaion-test-plugin` is a made-up engine served as a plugin, never shipped; its
  `tests/protocol.rs` runs the protocol end to end (results, credit, cancel, a plugin that
  dies, another version, the password in errors, logs, arguments and environment): any change
  to the three crates needs its test there. In the core, `plugins.rs` decides which program is
  started: one `Plugin` per file for the whole program (`plugin_at`), and
  `SPELAION_DEV_PLUGIN_<ENGINE>` points a development build at a plugin built on this machine;
  a release build ignores it (`dev_plugin`), and nothing in a setting or a repository file can
  name a plugin.
  `plugins/pins.rs` holds the pins (ADR-042): `plugins.toml` at the root, compiled into the
  core, names the builds of our plugins this repository tests and ships. Only a pinned program
  next to the app is ever a plugin (`Engines::with_pinned_plugins`), and its SHA-256 is checked
  every time its process is about to start (`check_file`, through the host's `with_check`): the
  one check, which the catalogue's plugins and a user's own will use with their own hash
  (ADR-055). Like `trust.rs`, any change there needs tests that try to get around it
  (`crates/spelaion-test-plugin/tests/pins.rs`).
  The catalogue of ADR-055 is three more files there, with no page, download or publishing
  workflow yet. `plugins/catalogue.rs`: a file shaped like `plugins.toml` (every version ever
  pinned, a serial, the withdrawn builds), read only after its minisign signature by the key
  compiled in (`SHIPPED_KEY`, whose secret half is only in the `plugins-catalogue` environment,
  usable only from `dev`; never the update key), and
  it carries no address: an archive's is written from the checked name and repository
  (`PinnedPlugin::archive_address`). `plugins/installed.rs`: the user's store
  (`<personal folder>/plugins`), which is not trusted: the kept catalogue is checked again at
  every read and never replaced by one with a lower serial, and an archive (`.tar.gz` on every
  platform) is installed only as the entry whose hashes it has, only its program unpacked.
  `plugins/choice.rs`: for each plugin, the newest build on this machine (next to the app or
  installed) that the pins or the accepted catalogue name, that speaks the core's protocol, that
  this app is not too old for and that was not withdrawn; when builds are here and none may be
  started, its engines say why and do not fall back; when the pinned build's program is not
  next to the app, an engine served only by its plugin says which file is missing (one whose
  driver is compiled in is still served by it). The app and `spelaiondb mcp` take their
  engines from `Engines::builtin_with(store)`; neither downloads. The installers carry the
  pinned builds as their release archives, never the programs (ADR-057: a signed macOS app and
  an AppImage change the programs inside): `builtin_with` first installs into the store each
  pinned archive found in the installer's resources (`plugins::bundled_folder`,
  `PluginStore::install_bundled`), with the store's checks. Any change to the three needs
  tests that try to get around it (`crates/spelaion-test-plugin/tests/catalogue.rs`, which signs
  its own catalogues with a key of the test).
  The catalogue is written, never edited: `Catalogue::next` gives the text that follows the
  published one from the pins, and **only adds** (a published version never changes, a
  withdrawn build stays withdrawn and is never pinned again, the serial is the next one).
  `plugins.toml` withdraws a build with a `[[withdrawn]]` entry and its reason; it cannot be the
  pinned build. `spelaion-write-catalogue` (in `crates/spelaion-plugin-fetch`, never shipped) is
  that function as a program for the publishing workflow: it must be told which catalogue is
  published (its one signed file, checked with the key compiled in) or `--first`, writes
  nothing when nothing is new, and signs nothing. `--check` is the last step before publishing:
  the signed file must be accepted with the key compiled in and hold the text written.
  `.github/workflows/plugins-catalogue.yml` runs it on `dev` when `plugins.toml` changes (or by
  hand), signs with minisign in the `plugins-catalogue` environment and publishes the release
  `plugins-catalogue`'s one asset (never the latest release, which the updater reads).
  `plugins/fetch.rs` brings the catalogue and an archive, and the core still has no HTTP
  client: the adapter gives a `Transport` that only moves bytes. No function there takes an
  address: the catalogue's is a constant and an archive's is the one the core writes for its
  entry. HTTPS where it starts and where it ends, a catalogue bounded and an archive of exactly
  its entry's size, a stalled download given up on, the file removed whatever happens, and what
  arrives goes through the same checks as a file the user picked. The catalogue is read only
  when `updates_allowed` (the user's setting; `SPELAION_NO_UPDATES` can only switch it off); an
  archive only on the user's click. Tests with a made-up network in the same `catalogue.rs`.
  `crates/spelaion-plugin-fetch` (`spelaion-fetch-plugins`, never shipped) is how the pinned
  builds get here, for CI, the release and a developer: it downloads each pinned release from
  its plugin's repository and lets a program into the folder it is given only if the archive
  has the pinned size and SHA-256, holds the program at its top and that program has the
  pinned SHA-256; then it prints the `SPELAION_DEV_PLUGIN_<ENGINE>` lines that send the tests
  through them. No secret, and no new dependency (`curl` and `tar`). Any change there needs
  tests that try to get around it (`tests/fetch.rs`).
  Until an engine's driver moves to its own repository, its crate here also builds its plugin
  program (`src/bin/spelaion-plugin-<engine>.rs`; MySQL's serves MariaDB too), and
  `scripts/measure-plugins.sh` measures ADR-041's gate (a million-row result and a million-row
  import, in process and through each plugin: at most 15 % longer), and
  `scripts/test-through-plugins.sh` runs any `cargo test` with every engine through those
  programs: a change to the protocol's three crates or to a driver is tested both ways. A
  database on another machine (SQLite over SSH, ADR-018) never goes through a plugin: the core
  keeps the driver compiled in for it (`ThroughPlugin` in `open.rs`).
- **PostgreSQL is a plugin** (ADR-041, ADR-042): its driver is not in this workspace. It lives
  in `spelaion-dev/spelaiondb-plugin-postgres` with its type zoo (each type's text identical
  to `psql`'s; a new type is added there first), its TLS and client certificates (ADR-022)
  and the patched `tokio-postgres`, and this repository pins the release it ships
  (`plugins.toml`). The plugin does the TLS handshake itself, against the host name the core
  gives it with the tunnel's local address. Here, its tests run through that plugin,
  unchanged, `spelaion-ssh`'s tunnel tests included: the pinned build next to the program, or,
  in a development build, the one `SPELAION_DEV_PLUGIN_POSTGRES` points at.
- **MySQL and MariaDB are a plugin** (ADR-041, ADR-042): their driver is not in this
  workspace. It lives in `spelaion-dev/spelaiondb-plugin-mysql` (one program serves both
  engines) with its type zoo and the patched `mysql_async` (`verify-ca` skips the name check),
  and this repository pins the release it ships (`plugins.toml`). What is the core's stays
  here: the dialect (`spelaion-dialect`'s `mysql.rs`), the session's `sql_mode`
  (`sql_mode.rs`), and how a MySQL string that holds a backslash is written
  (`export::sql_literal`, `objects::ddl::mysql_string`). `DATE`, `DATETIME` and `TIMESTAMP`
  columns arrive as text with their native type (ADR-005 and ADR-006, 2026-11-12 notes): what
  needs them as dates reads them by native type through `temporal.rs`. Here, their tests run
  through that plugin, unchanged: the pinned build next to the program, or, in a development
  build, the one `SPELAION_DEV_PLUGIN_MYSQL` and `SPELAION_DEV_PLUGIN_MARIADB` point at.
- `crates/spelaion-sqlite-schema` (ADR-058): what every program that talks to SQLite shares,
  with no `rusqlite`, network or runtime: the catalog queries and how their rows become the
  contract's model (`catalog.rs`), and the dynamic typing (ADR-006, `typing.rs`: affinity,
  what the first batch holds, the type decided, `ResultBuilder` from storage-class values to
  Arrow batches). The SQLite driver's two transports and the libSQL plugin are built on it, so
  they show the same explorer and types; plugins take it by the plugin SDK's tag.
- `crates/spelaion-driver-sqlite`: `rusqlite` in one thread per connection (an actor).
  Its catalog and typing are `spelaion-sqlite-schema`'s; `exec.rs` only reads rusqlite's
  values into it. Its tests need nothing external.
  **It stays compiled into the core** (ADR-041, "SQLite stays compiled in"): the core links
  SQLite for its store anyway, and the driver for a file on a server (`remote.rs`) is built on
  the same catalog queries and typing. Its plugin program (`src/bin`) is never shipped: it is
  how the protocol is tried with a real engine in this repository.
  `remote.rs` opens a file on a server (ADR-018) by running that server's `sqlite3 -safe`
  through `ConnectParams::remote` (an SSH exec channel on the tunnel's last hop,
  `spelaion-ssh::Tunnel::exec`): it must never let a dot-command or an incomplete statement
  reach the shell. Catalog queries and typing are `spelaion-sqlite-schema`'s. Tests against
  `testing/docker/sqlitehost` with `SPELAION_TEST_SSH`.
- **SQL Server is a plugin** (ADR-041, ADR-042): its driver is not in this workspace. It lives
  in `spelaion-dev/spelaiondb-plugin-sqlserver` with its tests and the patched `tiberius` it
  needs for cancel (a TDS Attention), and this repository pins the release it ships
  (`plugins.toml`). T-SQL batches are still split here (`spelaion-dialect`'s `sqlserver.rs`):
  the plugin receives each batch as one statement and may return several result sets. Here,
  its tests run through that plugin, unchanged, with `SPELAION_TEST_MSSQL=127.0.0.1:11433`:
  the pinned build next to the program, or, in a development build, the one
  `SPELAION_DEV_PLUGIN_SQLSERVER` points at.
- **MongoDB is a plugin whose language is not SQL** (ADR-056): its driver lives in
  `spelaion-dev/spelaiondb-plugin-mongodb` (MongoDB and FerretDB), pinned in `plugins.toml`.
  Its language is the core's, in `spelaion-dialect`'s `mongodb.rs`: a closed subset of the
  shell's method calls, never run as JavaScript. `Dialect::reads_requests` marks such a
  language: the session turns each statement into its command document (`Dialect::to_request`)
  and sends it with `Connection::execute_request` (protocol 1.1), never the text; the guard reads
  the command (`Dialect::request_class`), so `db.c.drop()` and `db.runCommand({drop: "c"})` ask
  alike. `Engine::is_sql` is false for it: a connection file with `read_only` or `[session]` is
  refused when it loads, and agents never see it (`AgentView::EngineWithoutBarrier`), until its
  own barrier exists. Tests: `spelaion-core`'s `tests/mongodb.rs` with
  `SPELAION_TEST_MONGO=127.0.0.1:27017` (or `SPELAION_DEV_PLUGIN_MONGODB` in a development
  build). A `mongodb+srv` address is resolved by the core (`mongodb_srv.rs`: its DNS seed
  list, then TLS always on), and the plugin is given one routed endpoint: a plugin never
  discovers addresses that would bypass the connection's route.
- **Redis is a plugin whose language is not SQL** (ADR-056), the same way: its driver lives in
  `spelaion-dev/spelaiondb-plugin-redis` (Redis and Valkey), pinned in `plugins.toml`. Its
  language is `spelaion-dialect`'s `redis.rs`: one command per line, quoted as `redis-cli`
  quotes, sent as its argument list (`{"args": [...]}`). The core refuses what would change or
  hold the connection (`AUTH`, `SELECT`, transactions, subscriptions, blocking commands); the
  tab's database is chosen above the editor. The guard's lists name the reads and the writes;
  `KEYS`, scripts and anything unlisted are unknown, never a read. The explorer shows each
  database's keys (kind `collection`, the key's type as its comment), read with bounded `SCAN`
  pages by the plugin, and opens one with a bounded command of its type
  (`Dialect::browse_statement`). Tests: `spelaion-core`'s `tests/redis.rs` with
  `SPELAION_TEST_REDIS=127.0.0.1:16379` (or `SPELAION_DEV_PLUGIN_REDIS`).
- **Cassandra is a plugin whose language is shaped like SQL but is not SQL** (ADR-056): its
  driver lives in `spelaion-dev/spelaiondb-plugin-cassandra` (Cassandra and ScyllaDB), pinned in
  `plugins.toml`. CQL is sent as text, as SQL is, but `sqlparser` never reads it: its splitter
  (batches whole), quoting and guard lists are `spelaion-dialect`'s `cql.rs`, through
  `Dialect::statement_class` (what the guard makes of a statement of such a language) and
  `Dialect::refuse_statement` (what the session never sends: `USE`, the keyspace being chosen
  above the editor). `Engine::is_sql` is false for it, so read-only mode, session setup and
  agents are not offered, and its tables open with the core's bounded statement
  (`Dialect::browse_statement`), never the table data tab, which writes SQL. Tests:
  `spelaion-core`'s `tests/cassandra.rs` with `SPELAION_TEST_CASSANDRA=127.0.0.1:19142` (or
  `SPELAION_DEV_PLUGIN_CASSANDRA`).
- **TDengine is a plugin whose SQL is its own** (ADR-056), the same way as CQL: its driver lives
  in `spelaion-dev/spelaiondb-plugin-tdengine`, pinned in `plugins.toml`, and talks to
  taosAdapter's REST API (TDengine's own Rust connector pulls an unmaintained TLS stack, and
  nothing of TDengine's AGPL code is linked). Its splitter, quoting (backticks) and guard lists are
  `spelaion-dialect`'s `tdengine.rs`: a `DELETE` without a `WHERE` deletes every row, `KILL` and
  the cluster's commands drop, `USE` is refused. Through REST a result arrives whole and a cancel
  only stops waiting. The explorer shows super tables as partitioned tables and at most 1,000
  child tables per database. Tests: `spelaion-core`'s `tests/tdengine.rs` with
  `SPELAION_TEST_TDENGINE=127.0.0.1:16141` (or `SPELAION_DEV_PLUGIN_TDENGINE`).
- **DuckDB is a plugin** (ADR-041, ADR-042): its driver is not in this workspace. It lives in
  `spelaion-dev/spelaiondb-plugin-duckdb` with its type zoo, and this repository pins the
  release it ships (`plugins.toml`). The core does not compile DuckDB. Here, DuckDB's tests
  run through that plugin, unchanged: the pinned build next to the program, or, in a
  development build, the one `SPELAION_DEV_PLUGIN_DUCKDB` points at (a build of the plugin's
  repository). Without either, a DuckDB connection fails saying which file is missing. One
  open per file shared by every session (ADR-016) holds because one plugin process serves
  them all.
- **ClickHouse only exists as a plugin** (ADR-041, stage 3): `spelaion-dev/spelaiondb-plugin-clickhouse`
  (its HTTP interface, one server-side session per connection that is never silently
  replaced, results as the server's `TabSeparatedWithNamesAndTypes` text), pinned in
  `plugins.toml`. What the core has of it: `Engine::Clickhouse`, `spelaion-dialect`'s
  `clickhouse.rs`, its rules in `classify.rs` (table functions that reach outside: `url`,
  `file`, `s3`, `remote`, `mysql`, `executable`…), `guard.rs` (`ALTER TABLE … DROP | CLEAR`)
  and its read barrier: no transactions, so the session's `readonly` setting
  (`readonly.rs`'s `clickhouse_read_only`), set once for agents' reads, exports, column
  statistics and read-only connections, and never set twice (a user whose profile is
  read-only cannot set it). It declares no transactions, no editing, no imports, no plans
  and no data search, and agents' writes are refused (`run_write` needs a transaction). Its
  tests: `tests/clickhouse.rs` with `SPELAION_TEST_CLICKHOUSE=127.0.0.1:18123` (and
  `SPELAION_TEST_SSH` for the tunnel); `SPELAION_DEV_PLUGIN_CLICKHOUSE` in a development build.
- **Trino and Presto are one plugin** (`spelaion-dev/spelaiondb-plugin-trino`, engines `trino`
  and `presto`: the same HTTP protocol, results polled page by page through `nextUri`, the
  session kept in the client), pinned in `plugins.toml`. One dialect for both
  (`spelaion-dialect`'s `trino.rs`, named `trino`, as MariaDB reads as `mysql`). Their read
  barrier is `START TRANSACTION READ ONLY`, which the server enforces for every catalog
  (`READ_ONLY_VIOLATION`), always rolled back (`readonly.rs`, the exports' `barrier`); the
  classifier refuses the `query` and `raw_query` table functions, which send their text to the
  database behind a catalog. The explorer's three levels are catalogs (its databases), schemas
  and tables. Declared off: the app's transaction controls (writes depend on each catalog's
  connector), editing, imports, plans and data search; agents' writes are refused; a read-only
  connection relies on the core's check (no session-wide read-only mode). Tests:
  `tests/trino.rs` with `SPELAION_TEST_TRINO=127.0.0.1:18080` and
  `SPELAION_TEST_PRESTO=127.0.0.1:18081`.
- **Oracle only exists as a plugin** (ADR-059): `spelaion-dev/spelaiondb-plugin-oracle`, on
  Oracle's pure-Rust thin driver `oracledb`, vendored and patched there (its
  `vendor/oracledb/PATCHED.md`: cancel, TLS checked against the host through a tunnel,
  nothing printed, no aws-lc), pinned in `plugins.toml`. What the core has of it:
  `Engine::Oracle` (`database` is the service name), `spelaion-dialect`'s `oracle.rs`
  (statements end at `;`, which is not sent; a PL/SQL unit keeps its `;`s until a line holding
  only `/`; `q'[…]'` strings), and `guard.rs`'s reading of a PL/SQL block it cannot parse
  (`oracle_unparsed_risk`: a risky word or dynamic SQL in an anonymous block asks; defining a
  unit asks nothing). It has **no read-only mode and no agents yet**: the configuration
  refuses `read_only` and `has_read_barrier` (`agent_view`, the form) leaves it out, because
  a function with an autonomous transaction writes from inside a `SELECT`, even in a
  read-only transaction (ADR-059's 2026-11-15 note); no exports, column statistics or data
  search either, which need that barrier. Its tests: `tests/oracle.rs` with
  `SPELAION_TEST_ORACLE=127.0.0.1:11521` (and `SPELAION_TEST_SSH` for TLS through the two
  bastions); `SPELAION_DEV_PLUGIN_ORACLE` in a development build.
- **libSQL only exists as a plugin** (ADR-058): `spelaion-dev/spelaiondb-plugin-libsql` (Hrana
  over HTTP, a stream as the session; the token is the password), pinned in `plugins.toml`. Its
  results and explorer come from `spelaion-sqlite-schema` by the SDK's tag, so they are
  SQLite's. What the core has of it: `Engine::Libsql` (no database: a server serves one per
  address), SQLite's dialect and guard. Like Oracle, **no read-only mode and no agents**
  (`has_read_barrier`; ADR-058, point 4: no session barrier, and a read-only token is the
  server's), and no cancel. Its tests: `tests/libsql.rs` with `SPELAION_TEST_LIBSQL=127.0.0.1:18095`
  (and `SPELAION_TEST_SSH` and `SPELAION_TEST_SOCKS` for the tunnel and the proxy);
  `SPELAION_DEV_PLUGIN_LIBSQL` in a development build.
- **Arrow Flight SQL is one plugin** (`spelaion-dev/spelaiondb-plugin-flightsql`): a generic
  `flightsql` engine for any server that speaks it, and `dremio` and `gizmosql` by name, its
  results Arrow straight from the server (results on another server are refused). Each reads as
  standard SQL under its own name (`spelaion-dialect`'s `standard.rs`: Trino's splitter, a name
  no barrier answers to). **They have no read barrier of ours** (`Engine::has_read_barrier`,
  false for them as for MongoDB and Redis): `read_only` is refused when the file loads, agents
  never see them (`AgentView::EngineWithoutBarrier`) and the form offers neither (the engine
  list's `read_barrier`). Tests: `tests/flightsql.rs` with
  `SPELAION_TEST_GIZMOSQL=127.0.0.1:31337` (GizmoSQL, as `gizmosql` and `flightsql`); Dremio is
  tested in the plugin's repository. **InfluxDB 3** is a kind of `flightsql` connection in the
  picker (`catalog.rs`): the plugin sends the connection's database as a `database` header on
  every call, which InfluxDB chooses its database by; `tests/influxdb.rs` with
  `SPELAION_TEST_INFLUXDB=127.0.0.1:18181` (no authentication: a token login is not done yet).
- **Neo4j is a plugin whose language is not SQL** (ADR-056): `spelaion-dev/spelaiondb-plugin-neo4j`
  (its HTTP Query API, user and password in HTTP Basic; each statement its own transaction),
  pinned in `plugins.toml`. Its language is `spelaion-dialect`'s `neo4j.rs`: Cypher split at `;`
  outside strings, backticked names and comments, each statement sent as
  `{"database", "statement"}` in the session's database. The guard reads the clauses: any
  `DELETE` asks as a delete, `DROP` as a drop; a write clause is a write, and a procedure
  (`CALL x.y`) and anything unlisted are unknown, never a read. Nodes, relationships, paths,
  lists and maps arrive as JSON cells. The explorer shows each database's node labels and
  relationship types (`ObjectKind::NodeLabel`, `RelationshipType`), opened with a bounded
  `MATCH` (`Dialect::browse_statement`). Like MongoDB and Redis, no read-only mode, no agents
  and no session setup, and no cancel. Tests: `spelaion-core`'s `tests/neo4j.rs` with
  `SPELAION_TEST_NEO4J=127.0.0.1:17474` (and `SPELAION_TEST_SSH` and `SPELAION_TEST_SOCKS`);
  `SPELAION_DEV_PLUGIN_NEO4J` in a development build.
- **Databend only exists as a plugin** (SDB-351): `spelaion-dev/spelaiondb-plugin-databend` (its
  HTTP query API: pages through `next_uri`, the session kept by the client, a cancel by
  `kill_uri`), pinned in `plugins.toml`. What the core has of it: `Engine::Databend`,
  `spelaion-dialect`'s `databend.rs` (Databend's own lexer: backslash escapes in `'…'` and
  `"…"`, backticks without escapes, block comments that do not nest, `$$` bodies, names quoted
  in double quotes), the guard through sqlparser's generic dialect, and its databases as the
  explorer's schemas and the editor's schema, as MySQL's (`objects/mod.rs`). Reached through
  MySQL's protocol instead, it is recognised by its version (`server_family.rs`, SDB-350).
  **No read-only mode and no agents**: it accepts MySQL's read-only modes and writes anyway
  (`has_read_barrier`). Its tests: `tests/databend.rs` with `SPELAION_TEST_DATABEND=127.0.0.1:18320`
  (and `SPELAION_TEST_SSH` and `SPELAION_TEST_SOCKS`); `SPELAION_DEV_PLUGIN_DATABEND` in a
  development build.
- **Spanner only exists as a plugin** (SDB-352): `spelaion-dev/spelaiondb-plugin-spanner` (its
  REST API, GoogleSQL: a session per connection, queries as single-use reads, `BEGIN`/`COMMIT`/
  `ROLLBACK` driving a read-write transaction, DDL through the admin API), pinned in
  `plugins.toml`. A connection's `database` is `projects/<p>/instances/<i>/databases/<d>` and its
  password an OAuth access token (a core secret: a `cmd:` secret may run gcloud). What the core
  has of it: `Engine::Spanner`, `spelaion-dialect`'s `spanner.rs` (GoogleSQL's lexer: backslash
  escapes, triple-quoted strings, `#` comments, backtick names), the guard through sqlparser's
  BigQuery dialect, and its default schema, which has no name, shown as `default` and left out of
  the names the core writes (`objects::qualified_name`). **No read-only mode and no agents** yet
  (`has_read_barrier`): Spanner enforces read-only transactions, but making them a barrier needs an
  ADR. No cancel (the REST API cannot). Its tests: `tests/spanner.rs` with
  `SPELAION_TEST_SPANNER=127.0.0.1:19021` (Google's emulator; the tests create its instance and
  database), and `SPELAION_TEST_SSH` and `SPELAION_TEST_SOCKS`; `SPELAION_DEV_PLUGIN_SPANNER` in a
  development build. Google itself is not tested.
- **HiveServer2's engines are one plugin** (`spelaion-dev/spelaiondb-plugin-hive`, SDB-176):
  `hive`, `spark` (Spark's Thrift server), `kyuubi` and `impala`, over HiveServer2's Thrift
  service (SASL `PLAIN`, or no SASL for Impala without a password; no Kerberos yet), pinned in
  `plugins.toml`. A Cloudera cluster's Hive and Impala are these engines. Each reads as HiveQL
  under its own name (`spelaion-dialect`'s `hive.rs`: backquoted names, strings in both quotes
  with backslash escapes), and the guard parses them with sqlparser's Hive dialect and also asks
  before what replaces data without deleting it first (`guard.rs`'s `hive_risk`: `OVERWRITE`
  anywhere, `ALTER TABLE … DROP`). **They have no read barrier of ours** (no read-only session
  or transaction they enforce), as for the Flight SQL engines: no read-only mode, no agents.
  Tests: `tests/hive.rs` with `SPELAION_TEST_HIVE=127.0.0.1:11010` (Hive, and `SPELAION_TEST_SSH`
  for the tunnel); Spark, Kyuubi and Impala are tested in the plugin's repository.
- `crates/spelaion-config`: TOML files from `.spelaion/` and the personal directory,
  variables, secret references and repository trust (ADR-009; `trust.rs` pins each repository
  connection and tunnel as trusted, and the resolver refuses one that changed, ADR-022: any
  change there needs tests that try to get around it). Every change to the trust store goes
  through `TrustStore::update`, one at a time, so none saves over another it did not see
  (`tests/trust_updates.rs`); `TrustStore::load` is for reading. If you change `model.rs`,
  regenerate the schema: `SPELAION_UPDATE_SCHEMA=1 cargo test -p spelaion-config`.
  `draft.rs` turns the add/edit connection form into a validated file (secrets go to the
  keychain, never to the file); editing touches only the changed keys with `toml_edit`.
  Every password written to the keychain goes through `secrets::store`, which reads it back
  (a store that answers `Ok` and keeps nothing is an error); never call `keychain_set` directly.
  A connection's `description` is the user's free text (the explorer's tooltip, and what
  `list_connections` tells agents a database is for) and its `tags` are short labels the
  explorer filters connections by (`normalize_tags` in `draft.rs`): both bounded and without
  hidden characters (`load.rs`), and like every key part of what a trusted repository pins.
  `tunnel_draft.rs` does the same for SSH tunnels (hops matched by position; a stored hop
  password is forgotten when that hop points to another server).
  Every file the configuration writes goes through `atomic_file` (temporary file + rename, the
  old mode kept, a symbolic link stays a link; new files owner-only in the personal folder and
  the umask's in the repository, `edit::new_file_mode`); never `std::fs::write` over a user's file.
  The core's exports (audit, history, plans, data files) stream through its `AtomicWriter`
  (a uniquely named temporary file, the same guarantees).
  Importing connections from other clients (ADR-039): `import.rs` is the flow every client
  shares (the `ImportedConnection` model, drafts, duplicates found by name or by the database
  they reach, replace in place, target folder, what was not mapped, passwords read from another
  app's keychain only on import); a client is a reader that returns `ImportedConnection`s.
  On Linux a reader's default search also looks in the client's Flatpak and Snap folders
  (`linux_dirs.rs`; native first, each package path citing the manifest it was checked against);
  HeidiSQL's registry only exists on Windows (`heidisql_has_registry`), elsewhere its import
  starts by choosing an exported file.
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
  `heidisql.rs` reads HeidiSQL's exported settings file and, on Windows, its registry
  sessions, with one reader for both: the format is untrusted (bounded inputs, malformed
  settings fail closed) and no password reaches an error or the preview.
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
  (ADR-022; the resolver passes it only when trusted and unchanged), then the user's. Each hop
  asks its server for the key types pinned for it first (`hostkeys::pinned_algorithms`, as
  OpenSSH does), so a file that pins only an ECDSA or RSA key still verifies the server; the
  check never changes. Dialogs go through the `Interaction` trait. Tests against the
  Docker bastions with `SPELAION_TEST_SSH=127.0.0.1:2221`. A tunnel may go through a
  SOCKS5 or HTTP proxy (`proxy.rs`, ADR-027): its first hop, or the database itself with no
  hops; tests against `testing/docker/proxy` with `SPELAION_TEST_SOCKS=127.0.0.1:11080` and
  `SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128`.
  AWS SSM (ADR-050) is a tunnel file's `[ssm]` table: `ssm.rs` runs `aws ssm start-session` with
  an argument list (values checked, options as `--name=value`) and stops the CLI and its plugin
  together (their process group on Unix, a job object on Windows), a close waiting until both
  have exited; `tunnel.rs` uses it like the proxy. Its tests run the stand-in
  `tests/support/fake_aws.rs` (the `spelaion-test-fake-aws` binary); a real AWS account is only
  tested by hand.
  Kubernetes (ADR-015) is a connection's `[kubernetes]` section, or a tunnel file's (a
  Kubernetes network profile, ADR-050: the resolver merges the two, each setting from one
  place, into the connection's `ResolvedKubernetes`):
  `spelaion-core::kube` runs `kubectl port-forward` and lists contexts, namespaces, names and
  ports for the form's Kubernetes tab; tests against a kind cluster with
  `SPELAION_TEST_K8S=<kubeconfig>` (see `testing/k8s`).
- `crates/spelaion-core`: `open_connection` (resolved configuration → tunnel from the
  `TunnelPool` if any → driver → `Session`), `Session` (splits and runs scripts: stop,
  continue or ask on an error, with a statement timeout and a summary; the "ask" pause and its
  ticket are in `script.rs`, ADR-035) and `ResultStore` (results in the core, served in Arrow
  IPC windows).
  `catalog.rs` lists the engines of the "add connection" picker (planned ones included); a
  server that speaks another engine's protocol is a kind of that engine's connection there
  (CockroachDB, YugabyteDB and TimescaleDB are PostgreSQL connections, SDB-169; TiDB, OceanBase
  and SingleStore are MySQL connections, SDB-170, and so is GreptimeDB, SDB-344, whose rows the
  MySQL plugin keys by primary key and time index, and an append-only table by nothing), and
  `server_family.rs` reads which server a PostgreSQL or MySQL session reached from the server:
  one that does not enforce its engine's read-only transaction or session mode gets no agents,
  streaming reads or read-only connections (unchecked PostgreSQL forks neither), and a
  read-only connection's session mode is read back from the server after it is set (a MySQL
  fork that names itself as MySQL, StarRocks, is caught there). A MySQL transaction's read-only
  mode cannot be read back, so a fork that accepts it and writes anyway must be recognised by
  its version (GreptimeDB; Databend, whose version only says MySQL and its own release, SDB-350).
  Like `readonly.rs`, any change
  there needs tests that try to get around it (`tests/postgres_family.rs` and
  `tests/mysql_family.rs`, against `testing/docker/pg-family` and `mysql-family`, and GreptimeDB
  in the main project's `warehouse` profile, which CI runs).
  `classify.rs` and `readonly.rs` are the read-only mode barriers (ADR-008): any change
  there needs tests that try to get around them. `guard.rs` lists the statements the app
  must confirm before running (DELETE/UPDATE without WHERE, DROP, TRUNCATE, and what the
  connection's type asks for: data changes on Production, every run where it confirms
  execution, ADR-029); `check_script` refuses unconfirmed risky scripts, so the UI never
  decides it. A read-only connection runs only reads, session settings and transaction control
  (ADR-053, `classify_read_only_connection`): `check_run` refuses anything else before the guard
  asks and whatever was confirmed, the session refuses it again (`Session::with_read_only`, set
  by `open_connection`), and `session_setup.rs` first sets PostgreSQL's, MySQL's and MariaDB's
  own read-only mode; `tests/read_only_connection.rs` tries to get around it on every engine.
  A MySQL/MariaDB statement with executable comments (`/*!50001 … */`, `/*M! … */`) is
  checked by the guard with their bodies both run and skipped, as the server's version decides.
  A MySQL/MariaDB session whose `sql_mode` reads quotes otherwise (`NO_BACKSLASH_ESCAPES`,
  `ANSI_QUOTES`) has each statement also read as that mode reads it (`sql_mode::as_default_mode`,
  from `Dialect::sql_mode`): the guard asks if any reading is risky and a read-only connection
  refuses unless every reading passes; the default reading is always one of them.
  A SQL Server batch the parser cannot read (T-SQL needs no `;`) is read token by token: a
  `DROP`, `TRUNCATE`, `DELETE` or `UPDATE` word in it asks. `health.rs` keeps idle sessions alive, marks a session lost (ADR-028) and
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
  only, stopped by a newer one or a cancel; `group.rs` is the grouping panel the same way:
  the rows of that view counted by some columns (NULL a group of its own, a masked column by its
  mask, a bounded answer), and "filter to a group" written as column filters of `filter.rs`
  from one row of the group; `chart.rs` prepares a chart's points from that view,
  ADR-040: at most `MAX_CHART_POINTS`, rows combined by X, and it writes the saved PNG or SVG;
  `er/` is the ER diagram, ADR-046: the model read from the explorer's catalog, its layered layout
  computed once per model and display options, and its Mermaid and DBML text, which must parse
  with their own tools: `tests/er_diagram.rs` writes `apps/desktop/scripts/fixtures/er` and
  `scripts/er-text.test.mjs` parses them; `SPELAION_UPDATE_ER_FIXTURES=1` writes them again)
  and `cursor.rs` finds the statement under the editor's cursor, says what Run will send
  (`run_target`, the Run button's label) and writes the text of the statements picked from a
  selection or from the cursor to the end (`pick_statements`, `from_cursor`): the rest written
  over with spaces, and returned only if the engine's splitter reads in it exactly those
  statements (`tests/run_from_cursor.rs`, on every dialect); its `outline` (one entry per
  statement, bounded) and `neighbour_statement` are the editor's outline panel and its next /
  previous statement, from the same splitter. `progress.rs` thins a run's row
  counts to a few a second for the app (a big result streams hundreds of batches a second;
  every other event and the final count go through as they come). `table_data.rs` writes and checks
  the paged SELECT of the table data tab (the filter typed by the user cannot change it, and the
  primary key orders it after the user's order, ADR-054), writes the `ORDER BY` of its header
  sort from the table's own columns (`table_order_by`: the UI names columns, never writes SQL),
  and writes "filter by this value"
  conditions into its WHERE from the typed cell. `query_page.rs` pages an editor query only when
  each page can be fetched exactly (ADR-054): one table, its primary key in the result, keyset
  pages after the last row shown, never OFFSET; anything else runs once as written. The grid's
  sort of a paged query (a list of `SortKey`s, as the stored result's) is read again from the
  server the same way: its columns order the pages first and may hold NULLs, which the core
  writes last in both directions on every engine, in the `ORDER BY` and in the comparison that
  fetches the next page alike.
  `tests/query_pages.rs` writes between pages on every engine.
  `load_rows.rs` is "Load every row" and a page or row past the ones loaded (SDB-386): keyset
  pages of a paged query in order, cancellable with whole pages kept, after an estimate of the
  rows and memory; a table data tab's page N is its numbered page and its last page is counted
  (`table_data.rs`). `transpose.rs` makes the transposed view, rows shown as columns, a result
  of its own. `tests/result_jumps.rs` runs both paths on every engine.
  `variables.rs` (SDB-383) writes the editor's `${name}` variables into a script before
  anything else reads it, so the guard, the read-only checks and masking see the text that
  runs: values are written in as they are, only in code (`Dialect::code_ranges`), one
  statement at a time after splitting, and a value that ends its statement, starts another or
  opens a string or a comment it does not close is refused. A name is only looked up in the
  values given (the tab's, over its connection's in `spelaion-store`'s `connection_variables`):
  never the environment, and `${env:…}` is not a name. Agents have no variables. Like
  `classify.rs`, any change there needs tests that try to get around it (`tests/variables.rs`,
  on every dialect).
  `result_tabs.rs` (SDB-378) is each editor tab's result tabs over the `ResultStore`: which
  results a run replaces is decided there, never in the UI. A run frees the results that are
  not pinned, a run into a new tab frees none, a re-run of one result takes its place when
  its new rows start, and past the user's limit the oldest unpinned result of an earlier run
  is closed; a result with unsaved edits is never freed by a run. A pin is only an entry no
  run frees: closing the tab frees it. A tab's name is the user's, else its statement's first
  `--` comment line as the engine's dialect reads comments (`statement_title`). The app's run
  commands place every result through `begin_run` and `RunResults::observe`
  (`tests/kept_results.rs` runs it against a real session).
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
  100,000 rows; `tests/column_stats.rs` runs it on every engine. `column_top_values` is the same
  count of the most frequent values alone, for the custom filter's value picker: a masked
  column is refused before anything is read (`tests/masking.rs`).
  `table_filters.rs` is what a table data tab remembers (the filters used on a table, and the
  default one its tab opens with): local data in the store, so every filter is checked like a
  page's before it is kept and again each time it is read, and a stored default that no longer
  passes is handed back as refused, never as a filter. Like `table_data.rs`, any change there
  needs tests that try to get around it.
  Server observation (the process monitor's other views): `observe.rs` runs each statement
  only after `classify_read_only` accepts it, as session metadata; `locks.rs` reads each
  engine's locks and waits and builds the blocking tree (ending a blocker always goes through
  `processes.rs`'s `control_process`), `server_settings.rs` the settings, variables and status,
  `table_stats.rs` a table's sizes and row estimate from the catalog; `tests/server_observation.rs`
  makes one session wait for another's row lock on every engine and checks the pair.
  `geometry.rs` (ADR-045) reads geometry values into GeoJSON for the cell panel's map: PostGIS's
  EWKB (its hexadecimal text), MySQL's SRID + WKB, SpatiaLite's and GeoPackage's blobs and
  DuckDB's WKB, as untrusted bytes (counts checked before allocating, bounded nesting, read to
  the last byte); `column_map` puts a column of the current view on one map, bounded.
  `tests/geometry.rs` converts each engine's points, lines and polygons (PostGIS in the test
  PostgreSQL's `spelaion_gis` database). The tile server is a personal preference
  (`spelaion-store`'s `map.rs`), off by default and checked before it is saved.
  `display.rs` (SDB-385) is how the grid shows values: `strftime` patterns for dates, times
  and timestamps, a number's marks, a display time zone (jiff; values without a zone are never
  converted) and how binary reads. Display only: `window_ipc` serves each cell as the database
  prints it and, next to it, as it is shown, and the grid draws the second; copies, exports,
  filters, value pickers, edits and Ctrl+F keep the database's text (`displayed_text` is the
  one copy of what is shown, asked for explicitly). A masked column is never formatted.
  `tests/value_display.rs` formats every engine's types and writes a filter from a formatted
  cell.
  `export.rs` formats copies and exports (HTML, IN lists, column names); `tests/grid_engines.rs`
  runs those conditions, lists and names on every engine.
  "Copy special…" (SDB-388) is `export.rs`'s `copy_special` (delimiter, quote, row delimiter,
  NULL text, header, row numbers; the choices are the copy defaults, `History::copy_defaults`,
  checked before they are saved), and copy as UPDATE is its `copy_updates`, through an
  `UpdateTable` only `edit.rs`'s `update_copy_table` makes, by the rule of edits (ADR-014:
  the table and its primary key from the catalog, rows read through this session, a query
  only as a plain SELECT of one table, an engine without transactions refused); a masked
  column in the key or the selection is refused, never written from the mask.
  `tests/copy_updates.rs` runs the copied UPDATEs on every engine and reads the rows back.
  `export.rs` also holds the CSV options and formula escaping of files;
  `export_stream.rs` streams a query, a table data tab or explorer tables to a file
  (ADR-034): only a single read is ever run again, in its own session and inside the engine's
  read-only transaction, one batch held at a time; like `readonly.rs`, any change there needs
  tests that try to get around it (`tests/export_stream.rs`).
  `pending.rs` keeps a table data page's pending edits (ADR-044): cells typed or changed in
  bulk, rows to delete and rows to insert, in the core with the stored result they refer to,
  each change a step undo and redo restore exactly; the stored result refuses to sort or filter
  while edits are pending, so a position keeps naming the same row (`tests/bulk_edits.rs`).
  `generate.rs` writes the explorer's "Generate SQL" for a table (SELECT by key or keys,
  INSERT, upsert or MERGE where the engine has one, UPDATE, DELETE) with the engine's quoting
  and `:named` placeholders; UPDATE and DELETE always carry a WHERE on what identifies a row
  (the primary key, else a unique constraint, else every column) (`tests/generated_sql.rs`).
  `plan/` is Visual EXPLAIN: each engine's reader (`postgres.rs`, `mysql.rs`, `sqlserver.rs`,
  `sqlite.rs`, `duckdb.rs`) turns its plan into one tree, analysed the same way; the plan's
  run (`run.rs`) is serialized and cancellable; `ANALYZE`, which runs the statement, needs a
  confirmation of its own and is off on a read-only connection; explicit runs are recorded in
  history; raw plan rows stay in the core, the page receives a bounded projection
  (`tests/plans*.rs`).
  `edit.rs` writes and applies the edits of a table data tab (ADR-014: primary key from the
  database, one transaction, one row per statement). Each stored result remembers the session
  that read it and that session's database (`ResultSource`; a statement that changed database
  leaves none), and edit statements are written only through an `EditTarget` from
  `Session::edit_target`, which refuses rows read through another session (another connection,
  a reconnected tab) or in another database; `tests/edit_source.rs` tries it on every engine.
  `masking.rs` is column masking (ADR-051): the session compiles the connection's rules
  (`spelaion-config::masking` combines its `[masking]`, the user's `user.toml` and its type's
  `mask_columns`; required patterns only add up) and `ResultStore` masks every batch as it
  arrives, keeping the originals aside for `StoredResult::reveal` only. Paths that skip the
  store (streaming exports, column statistics, the data search, `run_read_only`) apply
  `Session::masking` themselves; core reads it parses go through `run_parsed`. Like
  `classify.rs`, any change there needs tests that try to get around it (`tests/masking.rs`).
  Rules also match each column's origin (`ColumnMeta::origin`, ADR-052): PostgreSQL resolves
  it in the catalog when `ExecOptions::origins` asks, MySQL/MariaDB and SQLite report it
  directly; SQL Server, DuckDB and remote SQLite do not (a known limit).
  `value_view.rs` is the cell viewer's value, read from the stored result when the viewer opens
  (past `VIEW_LIMIT` only with "load anyway"): images recognised by their bytes (PNG, JPEG, GIF,
  WebP only, handed over as a `data:` URL; never SVG or HTML), XML pretty-printed as text, a value
  saved to a file byte for byte, and a file loaded into a cell as one `edit.rs` UPDATE by primary
  key; `tests/value_files.rs` runs it on every engine.
  `value_pick.rs` is the cell panel's value pickers (SDB-58): a date and time written with the
  column's precision as the grid shows it (UTC `+00` for a time zone, except SQL Server's
  `datetimeoffset`, written with its offset and up to seven digits like its `datetime2` and
  `time`; its `datetime` and `smalldatetime` rounded as the server does), the allowed values of an enum, a `SET` or a
  `CHECK (… IN (…))` read from the catalog (SQLite's from its `CREATE TABLE` text) only when
  exactly one list can be read reliably, and a foreign key's search: a checked page of the
  referenced table (never all of it), its filter written like "filter by this value". A pick is
  only a cell edit's text, applied by `edit.rs`; `tests/value_pickers.rs` reads every pick back
  on every engine, and searches a million-row table.
  `import/` loads a file or pasted text into a table (ADR-038): `source.rs` reads CSV, TSV, JSON
  arrays and Markdown tables as untrusted input (bounded records, columns and whole reads),
  `detect.rs` detects types and converts the few values servers disagree on, and `mod.rs` writes
  the DDL, checks read-only and the guard, and loads the rows in transactions through
  `Connection::insert_rows`, which takes them as columns of text (`TextRows`: Arrow, read as
  `&str`, never one `String` per cell, so they reach a plugin as they are) and binds every value (`COPY` on PostgreSQL, prepared inserts on
  MySQL, a TDS bulk load on SQL Server, the appender on DuckDB): never SQL text.
  `tests/import.rs` loads a million rows on every engine; any change there needs tests that try
  to get around it.
  `objects/` writes the explorer's object actions (ADR-026: qualified names, rename, drop,
  create, new index…) and the structure forms (ADR-047): `alter.rs` turns a form's columns into
  one script from the catalog read again each time, applied only as it was shown; `rebuild.rs`
  is SQLite's table rebuild; `create.rs` is the create table form; `source.rs` writes a view's or routine's
  definition as the statement that replaces it; `ddl.rs` edits the `CREATE TABLE` text SQLite and MySQL keep.
  `tests/object_actions.rs` runs each engine's statements on that engine.
  `objects/users.rs` (ADR-048) creates roles, users and logins, changes their password, locks
  and drops them: the script shown and compared has `'***'` for the password, the real one is
  written only into the statement that runs (never recorded) and scrubbed from its errors; any
  change there needs tests that try to leak the password (`tests/users.rs`).
  `objects/privileges.rs` grants and revokes privileges globally, on a database, a schema, an
  object or a role's membership (only the privilege names the core lists for that scope, never
  typed SQL), and reads what an account has (PostgreSQL's ACLs, MySQL's `SHOW GRANTS`, SQL
  Server's permissions); `tests/privileges.rs` tries each grant by logging in as the account.
  `backup/` (ADR-049) runs the vendors' own tools against a connection's route
  (`open::open_route`: the shared tunnel's or Kubernetes forward's local end, no session):
  `tools.rs` finds `pg_dump`, `pg_restore`, `psql`, `mysqldump` and `mysql` (a chosen folder,
  else `PATH` and the installers' places) and reads their version, `args.rs` writes each tool's
  argument list (never the password: it goes in `PGPASSWORD` / `MYSQL_PWD`), `process.rs` starts
  the tool without a shell, writes its output to `<name>.part` (renamed when it succeeds), feeds
  a restore's file to it and passes every log line through the connection test's `Redactor`.
  A restore is refused on a read-only connection and unless confirmed. `embedded.rs` is SQLite's
  `VACUUM INTO` and DuckDB's `EXPORT` / `IMPORT DATABASE` in a session of their own, and
  `run_file.rs` runs an `.sql` file there (ADR-036's decoding, the engine's splitter, statements
  done of the total, cancel; refused read-only or unconfirmed), tested in `tests/backup_files.rs`.
  `tests/backup.rs` dumps
  and restores PostgreSQL, MySQL and MariaDB through the two hops of `testing/docker` with the
  tools of the official images (`docker run`), or those in `SPELAION_TEST_DB_TOOLS`; any change
  there needs tests that try to leak the password.
  `conn_test.rs` is the connection test (ADR-032): one step at a time (resolve, proxy, each
  hop through `Tunnel::connect_observed`, Kubernetes, reaching the target with
  `Tunnel::probe`, login), its own tunnel, Stop that closes what it opened, failures classified
  there and a `Redactor` every label, message and the diagnostics text pass through: any change
  there needs tests that try to leak a secret (`tests/conn_test.rs`). A connection's
  `connect_timeout` is the database's (`ConnectParams::connect_timeout`, held whole by
  `open.rs`); a tunnel file's is per SSH hop.
  `panics.rs` (SDB-163): a panic is a bug, never swallowed. `install_panic_log` (the app and
  `spelaiondb mcp`) writes where each one happened to the app log, its message redacted and cut
  short; `catch_panic` turns one into an error for whoever waited. A run whose stream panics,
  or whose driver's stream ends without `Done` (an actor that died halfway), ends as a failed
  statement and the session is lost (ADR-028), never as a finished one (`tests/panics.rs`); an
  import whose reader panicked commits nothing more; an MCP tool call that panics answers an
  error and is audited as failed.
  `startup.rs` is what the desktop app says when it cannot start (SDB-283): no personal folder,
  a local store it cannot open (a newer version's is never started over: it says to update) or
  no window; the app shows it in a native dialog (`apps/desktop/src-tauri/src/startup.rs`, never
  a panic on the way to the window) and writes it to `app_log.rs`'s log
  (`<personal folder>/logs/spelaion.log`). "Start with a new local store" is
  `spelaion-store::set_aside`: the store and its `-wal`/`-shm` renamed to
  `state.db.broken-<date>`, never deleted or overwritten.
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
  `file_folder.rs` (SDB-316) is a folder of CSV, TSV and Parquet files as a DuckDB connection
  (`file_folder = true`): the resolver makes it an in-memory database, always read-only, and
  `open_connection` (and the connection test) set its session up before anything else runs:
  `allowed_directories` is the folder alone, external access off, a view per file DuckDB's
  `glob` lists (names quoted by the dialect, paths as literals), then `lock_configuration`.
  Writing to the folder is refused by the read-only barrier, and agents never see it
  (`AgentView::FileFolder`). Any change there needs tests that try to read outside the folder
  or write to it (`tests/file_folder.rs`).
  `write.rs` holds the barriers and the transaction of agents' writes (ADR-019, ADR-020: one
  statement, never destructive, only the kinds on its allow list). Each engine's read barrier
  is in `readonly.rs` (ADR-020) and is tested in `tests/agents_engines.rs`; like
  `classify.rs`, any change there needs tests that try to get around it.
- `crates/spelaion-https`: the HTTPS transport the app gives `spelaion-core::plugins::fetch`
  (ADR-055): the updater's `reqwest`, HTTPS only (a redirect too), the system's proxies, no
  cookies or credentials. It only moves bytes: what is fetched and kept is the core's. Tested
  against local servers; `-- --ignored` also fetches a pinned archive from GitHub. The Plugins
  page's commands are `apps/desktop/src-tauri/src/plugins.rs`: they take a name, a version or a
  picked file, never an address (`scripts/plugin-commands.test.mjs`), and swap `AppState.engines`
  after each change, so the next connection starts the new build without a restart. The page
  itself is `src/components/PluginsSettings.tsx` (Settings → Plugins) and only shows the core's
  overview; `src/plugins.ts` shares it with the "add connection" picker and a connection that
  could not open (`MissingPlugin.tsx`), which send to the page when an engine's plugin is not
  installed.
- `crates/spelaion-store`: local SQLite (audit, query history, saved queries, personal settings,
  the user's editor snippets and the template of new scripts in `editor_snippets.rs`, kept as
  personal settings and checked by `spelaion-core`'s `editor_snippets.rs` when saved and when
  read: bounded, plain text by `spelaion-config::check_plain_text`, the rule of a connection's
  description; they are text the editor inserts, never run),
  the explorer's filters and bookmarks, the editor's open tabs per workspace and recent files in
  `tabs.rs`, each table's filter history and default filter in `table_filters.rs`; later the
  schema cache). A new migration is appended to `MIGRATIONS` and its test finds it by its text,
  never by "the last one". `spelaion-core::QueryHistory` owns its desktop actor and wraps
  session execution to record each executed statement before its completion is delivered.
- `crates/spelaion-mcp` + `apps/cli`: `spelaiondb mcp` and utilities (`connections`, `trust`,
  `audit`, `check`: the team repository's CI check, from `spelaion-config::check`, ADR-022). In `spelaiondb mcp`, stdout is the protocol: no `println!`. Reads and writes
  (`execute_write`, only with `[mcp] writes = "allow"`) use separate sessions. What agents may
  do on a connection is `spelaion-core::agent_access` (`write.rs`): the user's read-only mode
  for every agent (ADR-033, `mcp.read_only` in the store, read on every call, failing closed)
  only takes writes away. Every tool call starts by reading the configuration and the trust
  store again (`Snapshot` in `lib.rs`): a change the user makes applies at the next call of a
  running server, never in the middle of one; a configuration with any problem exposes nothing;
  a cached session is reused only while its connection is exposed and its `Definition` (what it
  is resolved from, secrets aside) is unchanged, and only while it is not lost (`Cached::usable`:
  its health, its connection, its tunnel); the call that meets a lost session fails and is never
  retried, and the next call opens a new one. Query rows go out as JSON or TOON (`format.rs`); any change there
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
  text). Every other command is `#[command]` (`crates/spelaion-desktop-macros`), never
  `#[tauri::command]` by hand: it adds `#[specta::specta]` and runs an async command's body
  through `src-tauri/src/panics.rs`'s `guard`, so a command that panics answers the page with an
  error instead of leaving it waiting (SDB-163); an async command therefore returns `CmdResult`.
  Sync commands are caught by the invoke handler (`panics::catch_sync`). Our own canvas grid (`ResultGrid.tsx`): past the browsers' 33,554,428 px height cap
  its spacer stops at `MAX_SPACER_H` and the scroll is scaled to the rows (`src/grid/scroll.ts`),
  so a row position is always read with `contentTop` and set with `scrollToContent`, never with
  the container's `scrollTop`. App commands (name, shortcut, menus) are
  registered once in `src/commandRegistry.ts` and run through `src/commands.ts`: the tab and
  file menus, the activity bar, the global shortcuts and the command palette
  (`CommandPalette.tsx`, Ctrl+Shift+P) all read it, and `scripts/commands.test.mjs` checks the
  menus against it. The palette's objects are only what the explorer already read (`catalog.ts`).
  In the SQL editor, completion, Ctrl+click and hover resolve a name with one reader of the
  statement (`src/sqlCompletion.ts`: `completeSql` and `resolveName`, tested by
  `scripts/sql-completion.test.mjs` and `scripts/sql-names.test.mjs`) over the catalog the tab
  read in the background (`useCompletionCatalog.ts`): never a query per key or hover, and a
  name that is not certainly one table or column shows nothing. They are hints: nothing is run
  or decided from them. Smoke test against the real app:
  `node apps/desktop/e2e/smoke.mjs` (after building the app and the CLI in one command, so
  nothing is built twice: `npx tauri build --no-bundle -- -p spelaion-desktop -p
  spelaion-cli -p spelaion-plugin-fetch`; a release build on 16 GB needs `CARGO_BUILD_JOBS=3`,
  see `CONTRIBUTING.md`).
  It drives WebView2 over CDP, so the whole test runs on **Windows** only. Its window is moved
  left of every monitor as it opens and never takes the foreground (`SPELAION_E2E_OFFSCREEN`, read only by
  `open_main_window`), so the user's real mouse and keyboard cannot reach it; never make a step
  depend on the window being visible or focused (`SPELAION_E2E_SHOW_WINDOW=1` shows it). On **Linux**,
  `node apps/desktop/e2e/smoke-linux.mjs` drives the same release build through
  `tauri-driver` and WebKitWebDriver (`e2e/webdriver.mjs`, no dependencies): a shorter pass
  (start, trust, a query on every engine, the real clipboard both ways, an SSH tunnel, quitting
  with a transaction open), with what it needs in `CONTRIBUTING.md`. CI runs this Linux pass
  (`.github/workflows/smoke-linux.yml`, not a required check) on every pull request and push
  to `dev` that touches more than documentation: a release build on
  ubuntu with `testing/docker` up, under Xvfb with Openbox, so it takes the X11 paths (`xclip`
  for the clipboard, `wmctrl` for the close request). A failure names the step and uploads the
  screenshots and the log (artifact `smoke-linux`). macOS has neither (no
  WebDriver for WKWebView). `e2e/processes.mjs` closes windows and lists processes for both.
  Automatic updates (ADR-021): `src-tauri/src/update.rs` (Tauri's updater plugin, reading
  `latest.json` from the latest GitHub release, so releases are never pre-releases;
  `SPELAION_NO_UPDATES` switches them off, as the smoke test does). Update artifacts are signed only in the release workflow
  (`tauri.release.conf.json`, secret `TAURI_SIGNING_PRIVATE_KEY`); never commit a private key.
  Installers: `npx tauri build` (NSIS + MSI on Windows). Releases: pushing a tag `vX.Y.Z`
  runs `.github/workflows/release.yml` (installers for every OS, the `spelaiondb` CLI, notes from
  `docs/releases/vX.Y.Z.md` plus every commit since the previous tag, from
  `scripts/release-notes.sh`, so commit subjects must read well in a changelog); the tag must
  match the version in `Cargo.toml` and `tauri.conf.json`. Pushing a branch `release-test/<x>`
  runs the same workflow as a rehearsal: the same installers and CLI archives, uploaded as the
  run's artifacts and checked by `scripts/check-release-plugins.sh` (each pinned plugin next to
  `spelaiondb` with its pinned hash). Its jobs only read the repository, never name a secret and
  build without update artifacts (`tauri.rehearsal.conf.json`); only the tagged jobs have
  `contents: write`. Pushing such a branch is a maintainer's step, never an agent's.

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
  SPELAION_TEST_CLICKHOUSE=127.0.0.1:18123 SPELAION_TEST_ORACLE=127.0.0.1:11521
  SPELAION_TEST_LIBSQL=127.0.0.1:18095 SPELAION_TEST_NEO4J=127.0.0.1:17474
  SPELAION_TEST_DATABEND=127.0.0.1:18320 SPELAION_TEST_SPANNER=127.0.0.1:19021
  SPELAION_TEST_GIZMOSQL=127.0.0.1:31337
  SPELAION_TEST_SSH=127.0.0.1:2221 SPELAION_TEST_SOCKS=127.0.0.1:11080
  SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128 SPELAION_TEST_MONGO=127.0.0.1:27017
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
  the smoke test: `node e2e/smoke.mjs` on Windows (drives the real app over WebView2 CDP),
  `node e2e/smoke-linux.mjs` on Linux (a shorter pass through WebDriver, see `CONTRIBUTING.md`).
- If something could not be tested (for example Pageant or WebKitGTK), say so explicitly.

## Development environment
- Windows 11 is the main machine. Linux is tried with WSL (`scripts/setup-wsl.sh`) as a
  first look; definitive measurements on real Linux.
- Test databases and SSH bastions: `testing/docker` (see its README).
