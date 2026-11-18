# Driver contract

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
