# ADR-061: Editing plain single-table query results

- **Status:** Accepted (implemented)
- **Date:** 2026-11-20
- **Supersedes:** the restriction to table data tabs in [ADR-014](0014-editing-table-data.md)
- **Builds on:** [ADR-029](0029-connection-types.md), [ADR-031](0031-transactions.md),
  [ADR-044](0044-pending-edits-in-the-core.md) and [ADR-053](0053-read-only-connections-refuse-writes.md)

## Context

A plain `SELECT` of a single table can show the same rows and key columns as a table data tab.
The desktop already lets users edit those rows from the editor grid. SDB-41 records the rules
the implementation uses to decide which results qualify and which rows an edit refers to.

## Decision

1. **The core decides which query results qualify.** The desktop asks `editable_query_table`
   once a run with a single result has finished. The connection and session must be writable.
   The core supports query result editing for the PostgreSQL, MySQL (including MariaDB), SQL
   Server, SQLite and DuckDB dialects.
2. **One plain `SELECT` projects columns from one table.** `editable_query_source` accepts `*`
   without modifiers or a list of direct, unqualified column names. The query can still
   filter, order or limit the rows. Joins, derived tables, common table expressions,
   set operations, grouping, `DISTINCT`, projected expressions and column aliases leave a
   result read-only. The source name has at most a schema qualifier.
3. **The table and key come from the catalog.** The core requires a single matching table or
   partitioned table. For unqualified names, PostgreSQL and SQL Server use the server's name
   resolution, MySQL and MariaDB use the current database, and SQLite and DuckDB require a
   unique catalog match. If PostgreSQL resolves an unqualified name to a temporary table,
   the result stays read-only. DuckDB refuses qualified names and unqualified names shared
   by a temporary table or view. The result must contain every primary key column, with
   each returned column present in the table and appearing only once. Views and keyless
   tables stay read-only. Using all values of a keyless row to match it could affect several rows.
4. **The stored result belongs to the session and database that read it (SDB-137).**
   `ResultSource` records the core's session ID and database context. The core checks this
   provenance when deciding editability and when generating statements for pending edits or
   a file loaded into a cell. A different connection or a reconnect gives the tab a new session
   ID. When a result's database context cannot be established, its source is unknown and the
   result is read-only. Table name resolution still uses the session and catalog at the time
   of the eligibility check. Preserving the resolved relation in the stored result is tracked
   in SDB-433.
5. **Positions refer to the view the user edited (SDB-138).** Pending edits belong to the stored
   result. While they are pending, the core refuses changes to its sort, text filter or column
   filters, including removing a filter. Appending a page is allowed when it preserves the
   positions of existing rows. `edit_statements` takes UPDATE and DELETE keys from the stored
   typed values at those positions.
6. **Changes follow the table data editing flow.** The core generates statements against the
   qualified table and reads its primary key from the catalog again. The read-only check and
   any required data-change confirmation apply before writing. ADR-014 and ADR-031 govern the
   edit transaction and the affected-row checks. After Apply, the desktop reruns the query to
   refresh that result.

## Verification

- [Query eligibility tests](../../crates/spelaion-core/tests/session_sqlite.rs) check a plain
  projection, missing keys, duplicate result columns, views, joins and ambiguous names.
- [Edit provenance tests](../../crates/spelaion-core/tests/edit_source.rs) cover different
  sessions and database changes. [Pending edit tests](../../crates/spelaion-core/src/pending.rs)
  cover filter and sort changes and the append of a page without moving edited rows.
- The [desktop smoke test](../../apps/desktop/e2e/smoke.mjs) edits a simple query result and
  checks that a join's result stays read-only.

## Revisit if

Users need to edit results from joins or views and the core can establish a single writable
target with a complete, unambiguous key for each row.
