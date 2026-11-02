# ADR-005: Driver model

- **Status:** Accepted
- **Date:** 2026-11-02

## Context
Many engines (relational and non-relational) must be supported without falling into the
lowest common denominator. Every engine, the UI and the MCP touch this model: changing it
late is very expensive.

## Decision
Each engine is a Rust crate that implements a common contract, organized in layers:

1. **Small contract:** connect, execute (returns a stream of events: result set, affected
   rows, notices, errors), cancel, explain and introspect.
2. **Declared capabilities** (`transactions`, `cancel`, `explain`, `editable_results`,
   `sql_dialect`…). The UI adapts to capabilities, never to `if engine == postgres`.
3. **Values:** a `Value` enum with common variants (Null, Bool, Int, Float, exact Decimal,
   Text, Bytes, Date/Time/Timestamp(tz), Uuid, Json, Array) and an escape hatch
   `Native { type_name, text, raw }`. Every column carries the native type (name + OID)
   besides the logical one. The common type is used to render and sort; the native one is
   never lost and goes back to the database intact when editing.
4. **Driver-driven schema:** the explorer receives nodes (`kind`, `label`, `icon`,
   `has_children`, actions). On top, a common relational model (tables, columns, PK/FK,
   indexes) for autocompletion, ERD and MCP.
5. **Plans:** the engine's raw plan + an optional normalized tree (operation, cost,
   estimated/actual rows, time, children).
6. **Dialects:** a per-engine description (keywords, quoting, statement splitting,
   functions) shared by the core and the UI.
7. **Result transport:** Arrow (see ADR-010).

Extensibility:
- Now: Rust crates compiled in, with a contract designed so it can travel as messages.
- Later: drivers in external processes with an LSP-like protocol (JSON-RPC over stdio),
  including a JDBC bridge for Oracle, DB2, Snowflake…
- WASM: ruled out (sockets, TLS and async in WASI are not worth it).

## Implementation (2026-11-02)
- Results travel as Arrow batches; each field carries in its metadata the logical type
  (`spelunk.logical`), the native type (`spelunk.native_type`) and its id
  (`spelunk.native_id`). `Value` is used for single cells (MCP, editing, tests), not to
  move results.
- Types with their own Arrow column: bool, int (Int64), float (Float64), bytes, date, time,
  timestamp(tz) in microseconds. The rest (decimal, uuid, json, interval, array and
  `Native`) goes as text, identical to what the engine's console shows.
- ±infinity in dates and timestamps is represented with `i32::MAX/MIN` and `i64::MAX/MIN`.
- `Done.rows_affected` is `None` for statements without a count (DDL, `SET`…).
- Deliberate differences from `psql`: a `bool` cell is shown as `true`/`false`, and `money`
  without symbol or separators (they depend on `lc_monetary`).

## Alternatives
External drivers from day one: they isolate failures, but add IPC and complexity before
they are needed.

## Revisit if
MySQL (the second network engine, v0.2) forces big changes to the contract: it is
redesigned before the third engine.
