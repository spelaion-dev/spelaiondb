# ADR-016: DuckDB driver

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
DuckDB is an embedded analytical database: a file (or memory) like SQLite, read with SQL
that comes from PostgreSQL's parser, and full of nested types (lists, structs, maps, unions).
Users open `.duckdb` files from notebooks and pipelines and want to browse them like any
other database.

## Decision
- **The `duckdb` crate with `bundled`:** DuckDB is compiled in from the sources inside the
  crate (no download at build time), the same version on every platform. Its Arrow is ours
  (arrow 60), so results come out of DuckDB as Arrow batches without copying row by row.
- **One thread per connection (an actor)**, as in SQLite: DuckDB is synchronous.
- **One open per file and process.** DuckDB locks its file: a second `open` in the same
  process fails, and the app opens several connections to one database (a tab each, the
  explorer). The first connection opens the database and keeps a root handle; the others
  are clones of it (`try_clone`). The file closes with its last connection. A file is open
  in one mode at a time: asking for read-only while it is open for writing (or the other
  way round) is refused with our own message.
- **Read-only connections** use DuckDB's `access_mode = READ_ONLY`.
- **Types** (ADR-005): booleans, integers up to 64 bits (signed) and 32 bits (unsigned),
  floats, blobs, dates, times and timestamps (any unit, with and without time zone) keep their
  own column. `UBIGINT`, `HUGEINT` and `DECIMAL` travel as exact text; intervals, lists,
  structs, maps, unions and enums as the text DuckDB's `CAST(… AS VARCHAR)` writes, rebuilt
  from the Arrow arrays (`format.rs`). The type zoo compares each one with DuckDB's cast.
- **Statements without rows:** DuckDB answers DDL and DML with a `Count` column and `SET`,
  `BEGIN`… with a `Success` column. For statements that are not queries (`SELECT`, `WITH`,
  `SHOW`, `DESCRIBE`, `PRAGMA`…), those become `Done { rows_affected }` instead of a result;
  `INSERT … RETURNING` still returns its rows.
- **Not in `spelunk mcp` yet** (superseded by ADR-020: the MCP serves DuckDB with
  `enable_external_access = false`). DuckDB's read-only queries can still read any local file
  (`read_csv`, `read_text`) and, with extensions, the network: the MCP's "read-only" would
  not mean "only this database". The app works with DuckDB; the MCP refuses it
  (`ReadError::Unsupported`) until the read-only mode also limits file and network access.

### Deliberate differences from DuckDB's console
As in the other drivers, cells use SpelunkDB's common text: blobs as `\x…` hex, dates before
year 1 with ` BC`, and top-level doubles as Rust writes them (`100000000000000000000`, not
`1e+20`). Inside nested values DuckDB's own forms are kept.

### Known gaps
DuckDB's Arrow export loses the offset of `TIMETZ` (shown as a time), sends `BIT` as bytes and
`BIGNUM`/`VARINT` as opaque bytes. They are rare; they will be read from DuckDB's own text if
someone needs them.

## Alternatives
- **Linking a DuckDB installed on the system:** smaller builds, but versions differ between
  machines and Windows users rarely have it.
- **Reading rows with DuckDB's row API:** it converts from Arrow anyway, and loses the same
  types.

## Revisit if
The MCP gets a sandboxed read-only mode for DuckDB (`enable_external_access = false` and a
locked configuration), or the gaps above matter to someone.
