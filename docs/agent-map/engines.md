# Engines

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
  The SQLite driver's `describe_table` reads column and table CHECK constraints from the stored
  CREATE TABLE in the catalog, including their names where given (`tests/sqlite.rs`).
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
  `SPELAION_DEV_PLUGIN_SQLSERVER` points at. LocalDB (ADR-068) is a SQL Server connection
  whose host is `(localdb)\Instance` (`spelaion-config`'s `localdb_instance`): the plugin
  starts the instance with `SqlLocalDB` and speaks TDS through its pipe, no user means the
  Windows account, the configuration refuses tunnels, ports and certificates to verify, the
  connection test has no address step, and the picker's `localdb` kind exists only on
  Windows. Its real test is the plugin's `tests/localdb.rs` on a Windows machine with
  `SqlLocalDB` (GitHub's runners have it).
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
  read-only cannot set it). It declares no transactions and no editing, and agents' writes
  are refused (`run_write` needs a transaction). Imports load into a table of our own put in
  place by one atomic rename (ADR-066: `import/staged.rs`; new tables only, `Atomic`
  databases only). Data search writes its
  ClickHouse forms (`toString`, `positionCaseInsensitiveUTF8`, `substringUTF8`; its `String`
  is searched as text) and binds `{pN:String}`. Its plans are estimated only
  (`plan/clickhouse.rs`, `tests/plans_clickhouse.rs`; no ANALYZE). Its
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
  read-only transaction (ADR-059's 2026-11-15 note), and undoes any session setting that would
  stop it (`DISABLE COMMIT IN PROCEDURE`, `READ_ONLY = TRUE`) with an `ALTER SESSION` of its
  own (the 2026-11-23 note; `tests/oracle.rs` keeps that evidence). Do not turn the barrier on
  with a session setting; no exports, column statistics or data search either, which need
  that barrier. Its tests: `tests/oracle.rs` with
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
