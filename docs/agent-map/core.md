# Core

- `crates/spelaion-core`: `open_connection` (resolved configuration → tunnel from the
  `TunnelPool` if any → driver → `Session`), `Session` (splits and runs scripts: stop,
  continue or ask on an error, with a statement timeout and a summary; the "ask" pause and its
  ticket are in `script.rs`, ADR-035; with `ScriptOptions::statement_savepoints`, the user's
  setting, a savepoint of the core's around each of the user's statements in a transaction on
  PostgreSQL rolls back only a failed one, ADR-086: never around transaction control, never in
  the history, `transaction::statement_savepoint` says which engines, `tests/script_runs.rs`)
  and `ResultStore` (results in the core, served in Arrow IPC windows).
  The `TunnelPool` shares one SSH chain between connections only when `tunnel_pool_key`
  (`open.rs`) is the same: the connection's layer, the folder whose profile it is, the teams'
  `known_hosts` it is checked against (ADR-075: two folders' profiles of the same name and
  settings, or one profile checked against different folders' host keys, are never one chain),
  its name, settings, proxy, SSM and hops. A new field of `ResolvedTunnel` that changes what
  the chain is or how it is verified goes into that key.
  A stored result's end is one type, `ResultEnd` (SDB-367, ADR-060): `Loading`, `Whole`, or
  `Partial(PartialReason)` (cancelled, failed, timed out, connection lost). Only `session.rs`
  sets it, where a statement's stream stops (and `end_on_panic` for a run that panics), and it
  announces it with `CoreEvent::ResultEnded` before the statement's own end; a partial result
  keeps its rows and is still sorted, filtered and charted as the rows it has (SDB-275). Ask
  `StoredResult::end()`, never the statement's messages. A freed result is `ResultFreed`
  (`ResultStore::stored`), one typed error with one text. `src/result_end_tests.rs` pins every
  ending.
  `catalog.rs` lists the engines of the "add connection" picker (planned ones included); a
  server that speaks another engine's protocol is a kind of that engine's connection there
  (CockroachDB, YugabyteDB and TimescaleDB are PostgreSQL connections, SDB-169, and so is
  RisingWave, SDB-302, which gets no agents: a view hides its table functions that reach
  other servers from the classifier, ADR-020's 2026-11-23 note; Materialize, SDB-303, which
  gets no agents either: its read-only transaction lets DDL through; and CrateDB, SDB-304,
  which has no transactions: like RisingWave, its editor sessions follow none, ADR-031's
  2026-11-23 note; TiDB, OceanBase
  and SingleStore are MySQL connections, SDB-170, and so is GreptimeDB, SDB-344, whose rows the
  MySQL plugin keys by primary key and time index, and an append-only table by nothing, and
  StarRocks, SDB-310, whose primary keys the plugin reads from `information_schema.columns`
  and whose imports and data search it refuses, since it prepares neither), and
  `server_family.rs` reads which server a PostgreSQL or MySQL session reached from the server:
  one that does not enforce its engine's read-only transaction or session mode gets no agents,
  streaming reads or read-only connections (unchecked PostgreSQL forks neither), and a
  read-only connection's session mode is read back from the server after it is set (a MySQL
  fork that names itself as MySQL is caught there). A MySQL transaction's read-only
  mode cannot be read back, so a fork that accepts it and writes anyway must be recognised by
  its version (GreptimeDB; Databend, whose version only says MySQL and its own release, SDB-350;
  StarRocks, a MySQL connection in the picker whose comment is only its release and commit, by
  asking its own `current_version()`, SDB-310). `ServerFamily::opens_transactions` is false where
  `START TRANSACTION` and `autocommit = 0` open nothing (CrateDB, GreptimeDB, StarRocks, and
  RisingWave's read-write `BEGIN`, SDB-304): the
  session does not follow transactions there, so manual commit is refused rather than offered.
  On a SQL Server connection it tells only Babelfish apart (`@@VERSION`, SDB-313, ADR-020's
  note): its transactions hold, but its T-SQL reaches PostgreSQL's functions past SQL Server's
  rolled-back transaction, so it gets no agents, read-only connections or streaming reads
  (`tests/babelfish.rs`, against `testing/docker/babelfish`).
  Like `readonly.rs`, any change
  there needs tests that try to get around it (`tests/postgres_family.rs` and
  `tests/mysql_family.rs`, against `testing/docker/pg-family` and `mysql-family`, and GreptimeDB
  in the main project's `warehouse` profile, which CI runs).
  `cleartext.rs` (SDB-396) decides whether a driver may answer MySQL's `mysql_clear_password`,
  which sends the password as typed: only over TLS that checks the server's certificate or
  through an SSH tunnel with hops (a proxy, SSM or Kubernetes alone is refused), asked in
  `open::connect_driver`, the one gate every connection goes through, before the driver is
  called; like `classify.rs`, any change there needs tests that try to get around it
  (`tests/cleartext.rs`). `shown.rs` narrows every list of databases a session gives to the
  connection's `databases` and its own database (and MySQL's and MariaDB's schemas, which are
  its databases), in `health.rs`'s `Tracked`: what is shown, never a permission
  (`tests/shown_databases.rs`). Behind a pooler in transaction mode the session is marked
  (`Session::is_transaction_pooled`) and `objects::set_active_schema` refuses with why; the
  PostgreSQL plugin then prepares nothing by name. `tests/pgbouncer.rs` runs PostgreSQL
  through `testing/docker`'s PgBouncer (`SPELAION_TEST_PGBOUNCER=127.0.0.1:16432`), a
  connection without the setting included, which must lose a statement there.
  `session_source.rs` (SDB-426) decides whether a session made from another one (a tab
  opened from the explorer, a details or diagram tab, the process monitor; the app's
  `connect_from_session`) may reuse that session's resolved connection: only while the
  `ConnectionDefinition` it was opened from (`spelaion-config`'s `definition.rs`, no secrets)
  is still the configuration's. After a saved edit the saved file is resolved again, and the
  typed password goes with it only to the same login on the same route (`carried_password`);
  otherwise the tab connects as a new one does. Nothing is run again (`tests/session_source.rs`).
  `classify.rs` and `readonly.rs` are the read-only mode barriers (ADR-008): any change
  there needs tests that try to get around them. A client command line (ADR-078, SDB-466) is
  refused wherever nothing resolves it: `classify.rs`'s `reject_client_command` (agents' reads,
  the read-only connection barrier, session setup's check) and `write.rs`'s `check_write` refuse
  a statement the engine's scanner reads as one, whatever the SQL parser would make of it, and
  the session refuses it (the dialect's `refuse_statement`) before every send path: a plan's
  EXPLAIN, COPY and a request included (`session.rs`). Only the editor's run will resolve
  commands (SDB-467). `tests/properties.rs` (SDB-156) generates
  them: a write or a refused function hidden in every place it can go is never a read, and a
  destructive statement never passes the guard unasked; it also checks the `:name` scanner
  (`params.rs`: only placeholders in code are found and bound, a value stays in its literal),
  that pasted text, any import file and any `.sql` file read without a panic, and that SQLite
  itself agrees with its splitter on where each statement ends. It also checks that the guard
  reads a statement's first word as the splitter does (ADR-065, SDB-452): a DROP, or an
  unreadable TRUNCATE, DELETE or UPDATE, first to the splitter is asked about, and a DROP asked
  about is first. What it has found and a card will fix is in `GUARD_KNOWN`, each entry let
  through only in its own shape; the fix takes its entry out. `guard.rs` lists the statements the app
  must confirm before running (DELETE/UPDATE without WHERE, DROP, TRUNCATE, and what the
  connection's type asks for: data changes on Production, every run where it confirms
  execution, ADR-029); `check_script` refuses unconfirmed risky scripts, so the UI never
  decides it. Its comments are the dialect's (`Lexical`, SDB-517/518): the first word is read
  after the comments the dialect's `code_ranges` leaves out, and the SQL parser reads the
  statement with them blanked, so neither a `#` the engine does not have nor a block comment
  nested on one side only hides a statement or invents one; never a comment list of its own. A
  read-only connection runs only reads, session settings and transaction control
  (ADR-053, `classify_read_only_connection`): `check_run` refuses anything else before the guard
  asks and whatever was confirmed, the session refuses it again (`Session::with_read_only`, set
  by `open_connection`), and `session_setup.rs` first sets PostgreSQL's, MySQL's and MariaDB's
  own read-only mode; `tests/read_only_connection.rs` tries to get around it on every engine.
  `session_read_only.rs` (SDB-395) puts one editor session behind the same barrier on request,
  only ever stricter (`tests/session_read_only.rs`). `tls_warning.rs` decides the warning about a
  TLS mode that does not check the server on a host that is not this machine, for the form, the
  connection test and the explorer (`tests/tls_warning.rs`).
  A MySQL/MariaDB statement with executable comments (`/*!50001 … */`, `/*M! … */`) is
  checked by the guard with their bodies both run and skipped, as the server's version decides.
  A MySQL/MariaDB session whose `sql_mode` reads quotes otherwise (`NO_BACKSLASH_ESCAPES`,
  `ANSI_QUOTES`) has each statement also read as that mode reads it (`sql_mode::as_default_mode`,
  from `Dialect::sql_mode`): the guard asks if any reading is risky and a read-only connection
  refuses unless every reading passes; the default reading is always one of them.
  A SQL Server batch the parser cannot read (T-SQL needs no `;`) is read token by token: a
  `DROP`, `TRUNCATE`, `DELETE` or `UPDATE` word in it asks. The guard's first word is the
  dialect's scanner's where the dialect has one (`Dialect::first_word`, SDB-455: SQL Server);
  otherwise `Lexical` reads it through the dialect's `code_ranges`. `health.rs` keeps idle sessions alive, marks a session lost (ADR-028) and
  closes the ones a connection type says are idle too long, never with a transaction open
  (ADR-029): every use goes through its `Tracked` connection, and
  nothing is ever reconnected or re-run there. `transaction.rs` (ADR-031) is an editor tab's
  transaction: the state always comes from the driver (`Connection::transaction_state`, read
  after every statement), manual commit opens one with the engine's `BEGIN`, smart commit is a
  connection type safeguard, and `Session::close_needs_confirmation` is the check before
  closing a tab, disconnecting or quitting. The state is one enum, `TransactionLifecycle`
  (`transaction/lifecycle.rs`, SDB-372, ADR-060): not followed, or followed with a
  `CommitMode` (auto, manual, switched by smart commit) and a `TransactionPhase` (idle, open or
  failed, with its statement count and its `IdleRollback`); it changes only through its own
  methods. `idle_transaction.rs` (ADR-081) is the connection type's
  `close_idle_transaction_after`: `Session::roll_back_idle_transactions` counts from the last
  use (`Tracked::idle_since`; keep-alives and state reads are not uses), never while a run is
  going, paused or cancelling or anything holds the connection, marks the phase warned before
  the end, then holds the session as Commit does (`control`, `Tracked::try_hold`), asks the
  driver and runs the engine's `ROLLBACK`; a rollback that cannot run marks the session lost.
  Its test is `tests/transactions.rs`'s idle rollback on every engine.
  `TransactionStatus` is a read-only view made from it, and `closing_asks` is the one rule for
  "must closing ask" (`closing_asks_from_last_state` for the quit question, which leaves out a
  lost session). The page gets `TransactionStatusItem` (the state and the core's answers) and
  derives nothing; core-written statements use `run_generated`, never
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
  `scripts/er-text.test.mjs` parses them; `SPELAION_UPDATE_ER_FIXTURES=1` writes them again;
  `er/saved.rs` is a saved diagram, ADR-084: an arrangement (scope, display options, table
  positions, notes, groups) kept in the store as a versioned JSON document of its own types,
  checked when saved and every time it is read back, never applied in part; `layout::arrange`
  places it: placed tables where they were, new ones laid out among themselves below them,
  gone ones where they were, notes wrapped and groups framed by the core so the image exports
  match; `tests/er_diagram.rs` saves one on every engine and opens it again after reopening the
  store)
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
  on every dialect). `resolve_script` (ADR-078, SDB-467) is the editor's run's reading: it
  walks the split script, `@set`/`@unset` change a copy of the values from their line on (the
  values given are never changed), each `@echo` is kept with the statement it comes before, and
  the command lines leave the text; every other statement is substituted with the values in
  force there, by the same checks. `missing_variables` is what the page asks for (not a name an
  earlier `@set` gives or an `@unset` removed). `script.rs`'s `with_echoes` yields each echo
  just before the statement after it starts (the trailing ones only when the run went through
  every statement). Any other `@` line stays in the text, for the session to refuse.
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
  composite, multi-level and self-referencing keys on every engine. A query's result follows
  keys too (SDB-93) once `query_key_table` matched it to one table (editing's rule,
  `edit::query_table`, without the read-only gate: following only reads); the result keeps
  that table, and `check_key_table`, called by the app's `follow_key` and `row_references`
  before anything else, refuses another table's keys or a table replaced since (its identity,
  SDB-433); a table data page is bound to its table too (SDB-440). A result with no table bound
  (a view's page) is read as a page of the table named, as before.
  `data_search.rs` (ADR-043) searches a value in the tables the user chose: one `SELECT` per
  table written from the catalog and the columns' types, the value always bound as a parameter
  (`Connection::execute_bound`), each a read in the engine's read-only transaction in a session
  of its own, bounded (rows per table, total hits) and cancellable; `tests/data_search.rs` runs
  it on every engine, 50 tables cancelled mid-way included. ClickHouse (SDB-324) has its own
  forms (`toString`, `positionUTF8`, `substringUTF8`, a float or decimal cast by the column's
  type, since it compares no float with a decimal); it has no unique key nor transactions, so
  its hits are filtered by their values and its reads need none. Where any other server does
  not enforce the read-only transaction the search is refused, except where ADR-091 checked
  that its own statements cannot write (GreptimeDB: base tables only, a fixed list of
  functions; a change to the template keeps to that list). Metadata search is the app's
  (`dbSearch.ts` over `catalog.ts`, which keeps each schema's columns once read).
  `schema_compare/` (ADR-070) compares two schemas of the same engine: `compare_support` says
  which sessions can (PostgreSQL, MySQL, MariaDB, SQL Server, SQLite and DuckDB proper; every
  other engine and fork with the reason), `snapshot.rs` reads a side only through the catalog
  reads the explorer and the structure forms use (`read_current` for tables), and `diff.rs`
  decides every difference in one place with the forms' rules (`same_type`, `typed`), pairing
  parts by name and then by what they are (SQL Server names constraints itself), and comparing
  view and routine sources with their own name and their own schema's qualifiers left out
  (`ddl::rename_defined`, `ddl::requalified`). `script.rs` writes the script that makes the
  target like the source from both snapshots, through the existing writers only (ADR-047's
  `alteration_plan` against the target's catalog, SQLite's rebuild included, `create_table_plan`,
  ADR-026's drops and `new_table_element_sql`, "Edit source"'s `replace_form`, and the engine's
  kept `CREATE TABLE` text on SQLite, MySQL and DuckDB), in an order the statements need (drops,
  alterations, new tables, columns, constraints, foreign keys, views and routines last), with a
  header of what cannot be undone, what is left in place and what is not scripted; SQL Server
  gets one batch per piece. It never runs anything; `tests/schema_compare.rs` runs known
  differences, and both round trips (without drops, and with drops keeping the target's rows),
  on every engine it serves.
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
  `other_database.rs` (ADR-074, SDB-493) is a PostgreSQL connection's other databases, every
  decision in the core. `connection_databases` answers what a connection opens: its own
  database first, then what the server lists (`list_databases` on its own session) narrowed
  exactly to the file's `databases` (or all of them with `all_databases`); a plugin or a
  server that cannot list them leaves `OwnOnly` with the reason. `open_other_database` opens
  a new session (never a switch) from the connection's resolved definition with only
  `params.database` changed and the default schema unset (it is the own database's), through
  `open_connection`, so the route (the same `TunnelPool` entry), the read-only mode, masking,
  the type and the setup statements are the connection's. It refuses, each with its own
  `OtherDatabaseError`: an engine or a server family that is not PostgreSQL itself (asked to
  the server, never the file), a pooler, a file that names no other database, a name it does
  not name (exactly: PostgreSQL's names are case-sensitive), a name the server does not list
  for the user (templates included), and a database another connection with the same login
  (`session_source::same_login`, or its file's host, port and user when it does not resolve)
  has as its own, naming that connection. Agents never come here: the MCP server opens the
  file's `database` only. `change_session_database` (SDB-505) is a tab's change of database:
  refused while its session runs a script (`OtherDatabaseError::Running`), then
  `open_other_database`, which also goes back to the connection's own database, as its saved
  file opens it, from a session on another. `file_opens_database` is whether a file still
  opens a database, for restored tabs (`open_tabs.rs`, `RestoreNote::DatabaseMissing`).
  `tests/postgres_other_databases.rs` opens one through the two-hop
  tunnel and tries each refusal; `spelaion-mcp`'s tests keep an agent on `all_databases` to
  its own database.
  Server observation (the process monitor's other views): `observe.rs` runs each statement
  only after `classify_read_only` accepts it, as session metadata; `locks.rs` reads each
  engine's locks and waits and builds the blocking tree (ending a blocker always goes through
  `processes.rs`'s `control_process`), `server_settings.rs` the settings, variables and status,
  `table_stats.rs` a table's sizes and row estimate from the catalog; `tests/server_observation.rs`
  makes one session wait for another's row lock on every engine and checks the pair.
  `dashboard.rs` (SDB-192) is the monitor's live dashboard: a sampler job the core owns
  (ADR-060), one `DashboardState` with its `DashboardPhase` (sampling, paused, failed, stopped)
  published on a watch and moved only by its task. It reads one statement per sample through
  `observe::read` (PostgreSQL's `pg_stat_activity`, `pg_stat_database` and WAL position, MySQL's
  and MariaDB's `SHOW GLOBAL STATUS`, SQL Server's `sys.dm_os_performance_counters`), turns
  counters into rates, keeps the last 360 samples in memory only, reads nothing while hidden
  (`set_visible`), holds its session weakly and stops when its handle is dropped. The desktop's
  `dashboard.rs` keeps one sampler per monitor session and only forwards its state;
  `tests/server_dashboard.rs` samples around known work on every engine.
  `geometry.rs` (ADR-045) reads geometry values into GeoJSON for the cell panel's map: PostGIS's
  EWKB (its hexadecimal text), MySQL's SRID + WKB, SpatiaLite's and GeoPackage's blobs,
  DuckDB's WKB and SQL Server's CLR serialization (MS-SSCLRT, SDB-213), as untrusted bytes
  (counts checked before allocating, bounded nesting, read to the last byte); `column_map` puts
  a column of the current view on one map, bounded. SQL Server's geometry and geography are
  serialized alike but geography stores latitude first: the plugin names a CLR column's type
  (`geometry`, `geography`), and that decides. An older plugin says `udt` for every CLR type:
  then a value at SRID 0 is geometry (geography always has an SRID), and one with an SRID is
  never guessed: it is not drawn, and `MapNote::SqlServerTypeUnknown` (the cell) and
  `ColumnMap::type_unknown` (the column) say why. A text cell whose whole value is well-known
  text (WKT, or EWKT with `SRID=…;`, as `ST_AsText` and `ST_AsEWKT` write it; SDB-214) is read
  too, in any column, with the same bounds (nesting, finite coordinates, one geometry and
  nothing after it).
  `tests/geometry.rs` converts each engine's points, lines and polygons (PostGIS in the test
  PostgreSQL's `spelaion_gis` database). The tile server is a personal preference
  (`spelaion-store`'s `map.rs`), off by default and checked before it is saved, its
  attribution's link too (`check_attribution_link`). `tiles.rs` fetches every tile (ADR-085):
  the page asks by view and `z/x/y`, the core builds the address from the stored server and
  refuses with tiles off; a disk cache next to `state.db` honours `Cache-Control`, `Expires` and
  `ETag` (7 days when unreadable) with conditional requests; two requests at a time, 300 per map
  view; only PNG, JPEG or WebP; `OPENSTREETMAP` is the one preset, chosen only by the user. The
  transport is `crates/spelaion-tile-http` (its User-Agent `TILE_USER_AGENT`, no referrer or
  cookies, `https://` or `http://` to this computer, redirects only to `https://`), tested
  against a local server in `tests/local_server.rs`: **no test ever asks an outside host**.
  `display.rs` (SDB-385) is how the grid shows values: `strftime` patterns for dates, times
  and timestamps, a number's marks, a display time zone (jiff; values without a zone are never
  converted) and how binary reads. Display only: `window_ipc` serves each cell as the database
  prints it and, next to it, as it is shown, and the grid draws the second; copies, exports,
  filters, value pickers, edits and Ctrl+F keep the database's text (`displayed_text` is the
  one copy of what is shown, asked for explicitly). A masked column is never formatted.
  `tests/value_display.rs` formats every engine's types and writes a filter from a formatted
  cell.
  `export.rs` formats copies and exports (HTML, IN lists, column names); `tests/grid_engines.rs`
  runs those conditions, lists and names on every engine. Its `sql_literal` writes every
  literal the core writes (bound `:name` values, edits, copies, filters): where a backslash
  escapes in a string (`spelaion_dialect::backslash_escapes`, checked against each splitter),
  `\` and `'` go escaped with one, so no value ends its literal early (SDB-435); MySQL's go
  as hex when they hold a backslash. No caller escapes values itself.
  "Copy special…" (SDB-388) is `export.rs`'s `copy_special` (delimiter, quote, row delimiter,
  NULL text, header, row numbers; the choices are the copy defaults, `History::copy_defaults`,
  checked before they are saved), and copy as UPDATE is its `copy_updates`, through an
  `UpdateTable` only `edit.rs`'s `update_copy_table` makes, by the rule of edits (ADR-014:
  the table and its primary key from the catalog, rows read through this session, a query
  only as a plain SELECT of one table, an engine without transactions refused); a masked
  column in the key or the selection is refused, never written from the mask.
  `tests/copy_updates.rs` runs the copied UPDATEs on every engine and reads the rows back.
  `export.rs` also holds the CSV options and formula escaping of files;
  `export_defaults.rs` (SDB-66) is what the export dialog offers when it opens: the last
  export's format, CSV, file name, gzip, SQL and JSON choices, saved as personal settings
  (`spelaion-store`'s `export.rs`) only once `ExportDefaults::check` accepts them (what an
  export would accept) and read back part by part, a part this version cannot use at its
  default (formula escaping is on for a new user, and a damaged flag reads as on);
  `table_copy/` (SDB-35, ADR-069) copies tables between connections: `types.rs` maps a source
  column (logical and native type) to a portable kind and that kind to each target engine's type,
  naming every loss (`Loss`); `values.rs` writes each value as the canonical text the target
  reads; `run.rs` plans and runs a copy: the source read through `export_stream`'s
  `read_only_transaction` (its own cancel, passed on from the job's), the rows loaded by the
  import's `resolve_columns` and `run_resolved` (so its guard, transactions, #473's cancel rule
  and ClickHouse's staging hold), binary and masked columns refused until left out. The read and
  the load run at once, so a copy within one embedded file is refused up front
  (`CopyError::SameFile`, by `Session::database_file`, which `open_connection` names). Any change
  needs its unit tests (`types_tests.rs`), the every-pair copy test (`tests/copy_engines.rs`,
  49 pairs of seven engines, data compared) and `tests/copy_same_file.rs`.
  `export_stream.rs` streams a query, a table data tab or explorer tables to a file
  (ADR-034): only a single read is ever run again, in its own session and inside the engine's
  read-only transaction, one batch held at a time; like `readonly.rs`, any change there needs
  tests that try to get around it (`tests/export_stream.rs`).
  Its formats (SDB-391): the text ones are `RowWriter`'s (with `SqlOptions`: rows per INSERT,
  the schema, keyword and name case, and an upsert in each engine's form by the primary key
  the core reads from the catalog, only for a table's export; `JsonOptions`: ISO dates, JSON
  columns as JSON; NDJSON), and Parquet and Arrow IPC are `export_binary.rs`'s, written a
  batch at a time (a Parquet row group per batch), typed from each column's logical and native
  type (a decimal with a declared precision is a `Decimal128`, otherwise exact text). gzip
  wraps the `.part` file (Parquet compresses its column chunks instead), and
  `FileNamePattern` names the files (`{table}`, `{schema}`, `{database}`, `{date}`, `{time}`,
  `{timestamp}`), made safe by `file_name`. `tests/export_formats.rs` reads every format back
  on every engine, runs an upsert twice and checks masked columns.
  XLSX (SDB-390) is `export_xlsx.rs`'s, streamed the same way: a sheet is a zip entry (the
  `zip` crate, deflate only) written as its rows arrive, strings inline (no shared-strings
  table grows), and past Excel's 1,048,576 rows a sheet goes on in `name (2)`. Numbers,
  booleans, dates, times and timestamps are real cells only when Excel holds them exactly (an
  integer up to 2^53, a decimal a double holds digit for digit, a date from 1900-03-01); anything
  else is its text, and a value longer than a cell holds stops the export naming the column.
  Explorer tables as XLSX are one workbook, a sheet each (`export_tables`' `export_workbook`).
  `pending.rs` keeps a table data page's pending edits (ADR-044): cells typed or changed in
  bulk, rows to delete and rows to insert, in the core with the stored result they refer to,
  each change a step undo and redo restore exactly; the stored result refuses to sort or filter
  while edits are pending, so a position keeps naming the same row (`tests/bulk_edits.rs`).
  `generate.rs` writes the explorer's "Generate SQL" for a table (SELECT by key or keys,
  INSERT, upsert or MERGE where the engine has one, UPDATE, DELETE) with the engine's quoting
  and `:named` placeholders; UPDATE and DELETE always carry a WHERE on what identifies a row
  (the primary key, else a unique constraint, else every column) (`tests/generated_sql.rs`).
  `plan/` is Visual EXPLAIN: each engine's reader (`postgres.rs`, `mysql.rs`, `sqlserver.rs`,
  `sqlite.rs`, `duckdb.rs`, `clickhouse.rs`) turns its plan into one tree, analysed the same
  way (ClickHouse's `EXPLAIN json = 1, indexes = 1, description = 1` is estimated only: no
  costs or rows, its indexes' granules tell a full read; `classify.rs` reads its option list
  so read-only connections and agents can explain); the plan's
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
  An editable query result also keeps the table it was matched to (`ResultTable`, SDB-433,
  ADR-061): the desktop's edit commands get their target and table only from
  `result_edit_table`, which refuses another table's name and an identity (`table_identity`)
  that changed since the result was found editable. A table data page is bound the same way
  when it runs the SELECT the core wrote for it (`table_pages.rs`, SDB-440): the session
  records each page statement it writes. `result_edit_table` refuses any result left unbound.
  Edits to a result must never trust a table name that the page passes.
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
  `copy_in.rs` (SDB-162) sends the data after a script's `COPY … FROM STDIN`
  (`Dialect::copy_data`) as a plain `pg_dump` file has it: the session gives only the statement
  to the guard, the read-only checks and the history, and loads the rows through
  `Connection::insert_rows` a chunk at a time (COPY's default text format only, its column list
  required), in a transaction of its own unless the user's is open, so a cancel half-way leaves
  nothing. `cursor.rs` keeps a `COPY`'s data with it when a selection runs. `tests/copy_from_stdin.rs`
  restores a `pg_dump` of every common type and dumps it again identically, streams a million rows
  and cancels half-way. psql's `\copy` line is a meta-command, not a `COPY`: it has no data
  (`copy_stdio` is `None` for it), so the session refuses it before anything is sent (SDB-437).
  `copy_stdio` reads the words of PostgreSQL's one scanner (ADR-065, SDB-453), the splitter's
  own: if they disagreed on a statement's first word, text after its `;` could be taken both as
  COPY data and as SQL (SDB-439).
  `import/` loads a file or pasted text into a table (ADR-038): `source.rs` reads CSV, TSV, JSON
  arrays and Markdown tables as untrusted input (bounded records, columns and whole reads), and
  `xlsx.rs` a sheet of an XLSX file (SDB-390): every part's declared size, compression and
  ratio checked before it is unpacked, never more unpacked than declared and its CRC checked,
  no DOCTYPE, bounded nesting, tags, sheets, styles, shared strings, rows and columns, formulas
  never evaluated (their saved value, else their text); any change there needs tests that try to
  get around it (its own unit tests craft the workbooks; `tests/xlsx.rs` goes through Excel and
  back on every engine),
  `detect.rs` detects types and converts the few values servers disagree on, and `mod.rs` writes
  the DDL, checks read-only and the guard, and loads the rows in transactions through
  `Connection::insert_rows`, which takes them as columns of text (`TextRows`: Arrow, read as
  `&str`, never one `String` per cell, so they reach a plugin as they are; a copy's binary
  column into a connection with `insert_bytes` is a column of bytes, sent as hex by
  `table_copy` and decoded by the load, SDB-464) and binds every value (`COPY` on PostgreSQL, prepared inserts on
  MySQL, a TDS bulk load on SQL Server, the appender on DuckDB, TabSeparated data in the
  request's body on ClickHouse): never SQL text. ClickHouse has no transactions: `staged.rs`
  (ADR-066) loads into a table of our own (`__spelaion_import_` + a random UUID, created
  without `IF NOT EXISTS`) and puts it in place with one `RENAME TABLE` or `EXCHANGE TABLES`
  (the plan's `finish`), drops it on any failure or cancel, refuses appends and databases that
  are not `Atomic`, and lists the ones left behind (`leftovers`) without dropping them.
  `tests/import.rs` loads a million rows on every engine; any change there needs tests that try
  to get around it.
  `objects/` writes the explorer's object actions (ADR-026: qualified names, rename, drop,
  create, new index…) and the structure forms (ADR-047): `alter.rs` turns a form's columns into
  one script from the catalog read again each time, applied only as it was shown; `rebuild.rs`
  is SQLite's table rebuild; `create.rs` is the create table form; `source.rs` writes a view's or routine's
  definition as the statement that replaces it; `ddl.rs` edits the `CREATE TABLE` text SQLite and MySQL keep.
  `tests/object_actions.rs` runs each engine's statements on that engine.
  `objects/maintenance.rs` (SDB-392) writes the explorer's maintenance actions (VACUUM, ANALYZE,
  refresh, TRUNCATE options, triggers, CHECK / REPAIR / OPTIMIZE, rebuilds, integrity checks),
  each run as shown in a tab of its own; `tests/object_actions.rs` runs each and refuses each on a
  read-only connection.
  `objects/duplicate.rs` (SDB-394) writes the copy of a table in each engine's own form and says
  what it keeps; `tests/object_actions.rs` checks what it keeps on every engine.
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
  `task/` (ADR-089, SDB-33) saves an export, import, dump, restore, embedded copy, DuckDB import
  or `.sql` file run as a named task and runs it again. `document.rs` is the versioned JSON
  document, written from the core's own request types (their serde derives are its format:
  `task/tests.rs` pins version 1, so a change to `ExportFormat`, `CsvOptions`, `ParseOptions`,
  `DumpOptions` and the like that breaks it needs a new version and a reader for the old one),
  checked when saved and every time it is read back (a document that fails is unreadable and
  runs nothing), and `check_secrets` refuses any text holding one of the connection's resolved
  secrets or a secret-named `key=value` in a tool's arguments (`secrets_of`, the `Redactor`).
  A task's tools are `Detect` or a folder, never a program. `run.rs`: `resolve_connection` is
  the one way the app and the CLI resolve a task's connection (a repository one from its folder
  alone with the personal layer; a personal one with the folders the task recorded, through
  `Resolver::with_open_folders`); `run_task` records the run, asks for a prompt password before
  anything opens, prepares the run with the functions the dialogs call (`rerun_check` and
  `script_risks`, `plan_import`, `preview_dump`, `preview_restore`, `plan_sql_file`), stops for a
  yes exactly where the run by hand asks (`Questions::Ask` answers the question and records
  nothing; `Questions::Refuse`, the CLI, records it refused or failed), runs it in a session of
  its own through the same job functions and records the ending with the log, every line
  through the run's `Redactor`. An export never writes over a file (`free_path`). The tests
  with databases are `apps/cli/tests/task.rs` (the app's run and the real binary).
  `conn_test.rs` is the connection test (ADR-032): one step at a time (resolve, proxy, each
  hop through `Tunnel::connect_observed`, Kubernetes, reaching the target with
  `Tunnel::probe`, login and session setup with the same safeguards as opening a session), its own
  tunnel, Stop that closes what it opened, failures classified
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
  `diagnostics.rs` (SDB-398) writes the report "Copy diagnostic info" and a failed screen's
  "Copy report" copy: the version, the system, the plugins, the engines in use and the app
  log's last lines. Every text that is not the app's own goes through its `Scrubber`: the
  connection test's `Redactor`, the audit's redaction, every text of every connection and
  network profile file, the user name, the home folder and other absolute paths, IP addresses
  and host names. The app sends nothing (the issue page opens only on a click, at a fixed
  address); any change there needs tests that try to leak a password, a host or a path.
  `launch.rs` reads what the desktop app is asked to open from outside (SDB-397): a folder,
  `.sql` files (checked as a picked file is), a connection to select, a plan; nothing there
  connects or runs. `instance.rs` keeps one app per personal folder: a later launch hands its
  arguments to the running one over a local port, with a token from an owner-only file in the
  personal folder; any change there needs tests that try to hand over without it.
  `startup.rs` is what the desktop app says when it cannot start (SDB-283): no personal folder,
  a local store it cannot open (a newer version's is never started over: it says to update) or
  no window; the app shows it in a native dialog (`apps/desktop/src-tauri/src/startup.rs`, never
  a panic on the way to the window) and writes it to `app_log.rs`'s log
  (`<personal folder>/logs/spelaion.log`). "Start with a new local store" is
  `spelaion-store::set_aside`: the store and its `-wal`/`-shm` renamed to
  `state.db.broken-<date>`, never deleted or overwritten.
  The app log (SDB-31) has a level per entry (`LogLevel`: error, warning, info; an entry written
  before levels reads as an error) and is what Settings › Logs shows: `AppLog::read` (both files,
  newest first, filtered by level) and `AppLog::export`. Every entry is scrubbed when written and
  again when read (`app_log::scrub`: `audit::redact_secrets`, then an unquoted value after
  `password=`, `pwd:`, `token=`…), so a password an error message carried never reaches the file,
  the viewer or an export. Callers still never pass SQL, data or secrets: the scrub is a second
  line. The desktop writes the start (version and system), and each connection that opens, has
  its login refused or fails, by id.
  `save_name.rs` keeps a save dialog's file name in step with the chosen file type (Linux's
  GTK dialogs, `apps/desktop/src-tauri/src/file_dialog.rs`).
  `sql_file.rs` reads and writes the editor's `.sql` files (ADR-036: UTF-8 with or without a
  BOM, UTF-16, Windows-1252, the file's line ending, temp file + rename through
  `spelaion-config::atomic_file`, which keeps the mode and follows links, a content hash that
  refuses to overwrite a file changed on disk); `open_tabs.rs` keeps the open tabs (`TabKeeper`
  writes them at most a second after a change) and checks them before they come back
  (`restore_tabs`): never results, secrets or session state, and nothing is run. A tab belongs
  to its connection's folder (ADR-075, SDB-501): `tab_scope` keeps a folder's connection's tab
  under that folder and a personal connection's, or one without a connection, under the
  window's empty key; `window_sets` splits a window's tabs that way, each with its place in the
  window (`open_tabs.window_position`), and gives every open folder a set, so a folder whose
  tabs were all closed keeps none while a closed folder keeps its own; `TabKeeper::update_window`
  writes them together (`Store::save_window_tabs`, one transaction). `restore_window_tabs` puts
  the open folders' and the window's sets back in the window's order (tabs kept before places
  existed after the others, folder by folder), each connection looked up only in the folder it
  was kept under. History and saved queries list every open folder's and the personal ones
  (`history_page`, `snippet_page` and the rest take the open folders' keys; each entry has its
  `workspace`).
  `notebook/` (ADR-087, SDB-36) reads and writes SQL notebooks: `format.rs` is the file format
  (a front matter block, `sql cell=<name>` fences, `<!-- cell -->` between two Markdown cells)
  and its round trip, which the tests hold (`write(parse(write(x))) == write(x)`); files go
  through `sql_file.rs`'s rules. `references.rs` expands a cell's `{{name}}` references into
  CTEs of the referenced cells' SQL, found with the dialect's `code_ranges`; a referenced cell
  must be one statement that `classify_read_only` reads as a query, so a reference never
  repeats a write; the expanded statement is what `check_run` and the history see. Never run a
  cell any other way. `markdown.rs` reads Markdown cells into blocks that the page and
  `html.rs`'s export draw, never as HTML (raw HTML stays text, links are not followed); its
  readers are bounded (depth, a cell's size, searches for closing marks) and a change keeps the
  `deep_or_huge_input_stays_bounded` test. `html.rs` writes the export with every text escaped,
  a no-script policy, at most `MAX_EXPORT_ROWS` per result from the stored (masked) results,
  and a chart only as a `data:` image that passed `chart::is_plain_svg`. Results are never
  written into a notebook file, and a kept notebook tab (`open_tabs.notebook`) holds its
  document as text only. Tests: the modules' own and `tests/notebooks.rs` (every engine).
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
- Lifecycles (ADR-060): a state the core has (a session's health, run and transaction, a
  tunnel, a stored result's end, a job's cancel and ending) is one enum with its transitions in
  the type or actor that owns it, published on a watch when something follows it, and passed
  to the adapters as is. Already explicit, leave as they are: `SessionHealth`, `ScriptStop`,
  `StepStatus`, `TableStatus`, `HostFile`, `InstallState`, `AgentAccess`, `StartupAction`.
  The implicit ones are SDB-366 to SDB-377, one pull request each, in the ADR's order; each
  starts with a test that pins today's behaviour.
- A session's run (`run_state.rs`, SDB-370): `Session::run_state()` is a watch of `RunState`
  (idle, running a statement, paused with its ticket, cancelling), sent only when it changes.
  A run the user sees (`RunKind::User`, `Generated`) holds a `RunGuard` from when it asks for
  the session until it lets go of it or is dropped; the core's own reads (`Metadata`,
  `Parsed`) hold none and never make the session busy (SDB-277). "Is it busy" is
  `RunState::is_busy`, everywhere: Commit, Rollback, the auto-commit switch and closing read it,
  and `Session::transaction_refusal` says what runs. The cancels are counted in `Runs`, the one
  place the state is worked out. A failed statement says how it ended with `StatementEnd`
  (failed, cancelled, timed out; `cancelled` and `timed_out` only mirror it, set by
  `StatementError::ended`), `ScriptFinished` is built by `CoreEvent::finished` from its
  summary, and the summary's `stop` says how every run ended: `None` only when it went through
  every statement its mode let it run; a failure or a cancel names its statement even when it
  was the last; `SessionLost` (the connection was lost while paused) is apart from `Closed`.
  Tests: `run_state.rs` and `run_state_tests.rs`, one per transition.
- Jobs (`job.rs`, SDB-368): every job takes a `JobCancel`, made by its caller before the
  session exists and bound by the job itself (`attach`, after requesting a session run, so the
  run sees the cancel); a cancel always goes through `Session::cancel`. How a job's result reads
  as a `JobEnd` (done, stopped at a limit, cancelled or failed with what it kept, refused) is
  written once there, per job; the app answers with `JobOutcome::from_end`. A read run again in
  a session of its own enters and ends the engine's read-only transaction through
  `export_stream::read_only_transaction`. A new job adds its conversion, not new flags.
  `JobRegistry` (SDB-369) lists a program's running jobs under ids it makes: `start(kind)` gives
  a `JobHandle` whose drop removes the entry (a panic too), and `cancel(id, kind)` refuses an id
  of another kind. The desktop names its kinds in `src-tauri/src/jobs.rs`.
  The settings file (SDB-31) is `settings_transfer.rs`: one JSON file (`format`
  `spelaiondb-settings`, `version`) of the store's portable rows and the personal `user.toml` and
  `connection-types.toml`, written through `AtomicWriter`. Never a connection or a secret: those
  move in ADR-088's encrypted file. An import checks the whole file before it writes anything:
  the rows go into a store in memory (`Store::open_in_memory`, the same triggers), then the
  core's own checks run over them (shortcut keys, snippets, the script template, the name pattern
  of new connections, the tile server, read from the rows themselves because the readers drop
  what they cannot use). Then the personal files, then the rows in one transaction; when the rows
  fail the files are put back. A setting this version does not know is left out and listed.
  Tested in `tests/settings_transfer.rs`.
