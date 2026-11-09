# Execution plans

In a PostgreSQL, MySQL, MariaDB, SQL Server, SQLite or DuckDB query tab, choose **Explain query** in the action column, editor menu or
command palette. The selected text, or the statement under the cursor, is planned on that
tab's existing session. Temporary tables, search paths and the current transaction are
preserved. Parameters use the same value dialog as an ordinary query.
Connection setup (ADR-037) has already run when that session opens; explaining a query
neither opens a replacement session nor reapplies that setup. The schema picker and any
skipped-setup warning remain the same tab's state.

The default estimates the query without executing it. **ANALYZE executes the statement**;
its separate warning must be accepted for every run. An analyzed write also needs the same
core confirmations as the underlying SQL. Smart commit classifies that underlying SQL:
an estimated UPDATE and an analyzed SELECT do not switch it to manual commit, while an
analyzed UPDATE does. An already manual session stays manual. No plan command reconnects,
retries, commits or rolls back the user's transaction automatically. The tab's statement
timeout override also applies. Explicit plan runs enter query history.

What runs on each engine:

| Engine | Estimated | ANALYZE (executes) |
| --- | --- | --- |
| PostgreSQL | `EXPLAIN (FORMAT JSON, VERBOSE TRUE)` | adds `ANALYZE TRUE`, and `BUFFERS` when ticked |
| MySQL | `EXPLAIN FORMAT=JSON` | `EXPLAIN ANALYZE FORMAT=TREE` (an operator tree as text) |
| MariaDB | `EXPLAIN FORMAT=JSON` | `ANALYZE FORMAT=JSON` |
| SQL Server | `SET SHOWPLAN_XML ON` | `SET STATISTICS XML ON` |
| SQLite (local or on a server) | `EXPLAIN QUERY PLAN` (one row per step) | none: the toggle is disabled and the core refuses it |
| DuckDB | `EXPLAIN (FORMAT JSON)` | `EXPLAIN (ANALYZE, FORMAT JSON)` |

BUFFERS exists only on PostgreSQL. MySQL and MariaDB share a dialect, so the ANALYZE syntax
comes from the engine the connection was opened as. Each flavor's ANALYZE is a syntax error on
the other one: a connection that names the wrong engine is refused by the server before the
statement runs. MySQL does not analyze every statement (a single-table UPDATE, for one); its
`<not executable by iterator executor>` answer is reported as such, and nothing ran.

SQLite has no ANALYZE that executes the query: the view says so beside the disabled toggle,
and a request that asks for it anyway is refused before anything runs.

Read-only connections permit estimated read plans only. ANALYZE is refused in the core,
including when a caller bypasses the disabled UI control. The policy comes from the actual
resolved open connection. PostgreSQL's driver `read_only` flag alone is insufficient.

SQL Server sends each plan SET in a separate batch on the same session as the query. The
driver serializes the entire operation and restores the setting after errors, cancellation,
or an abandoned request. If it cannot restore the setting, it closes that session. An already
active SHOWPLAN or STATISTICS XML mode is left untouched and the plan request is refused.
ANALYZE discards ordinary query results while streaming; it does not run the statement again.
Errors after an analyzed execution may mean its effects remain in the current transaction.
SQL Server requires SHOWPLAN and statement permissions. Some statements do not produce an
actual XML plan, and documents containing multiple statements are currently unsupported.
ANALYZE also refuses unexpected extra result sets (for example, a trigger returning rows),
because their XML could impersonate a plan. The statement may already have run when refused.

SQL Server's runtime rows, executions and physical reads are summed over reported threads.
Elapsed thread times overlap in parallel, so they are shown only when a step reports a single
serial thread; inclusive times include children, and serial self time is shown only for a
leaf. Operators can omit runtime information; absent values remain unavailable. Sort spill
warnings come directly from the server. Saved Showplan XML supports UTF-8 and BOM-marked
UTF-16; saving an imported UTF-16 plan preserves its original encoding and byte order.

## Reading a plan

- **Overview** shows cost, planning/execution time, comparable self work, row estimate gaps,
  operation counts, depth, relations and indexes. Findings link to their steps.
- **Tree & table** sorts siblings while keeping parents before their children. All optional
  metrics are columns; collapse branches or filter to steps with findings. CSV exports the
  current table view, including all metric columns, using formula protection for text and
  headers by default.
- **Graph** presents the same bounded node page and selectable inspector. Edges connect
  visible parents and children; a parent outside the page has a navigation link.
- **Raw output** displays the original plan in UTF-8 windows. **Save raw plan** writes the
  original document. Opening a saved plan creates a local viewer, never an executable SQL tab.

Use the existing **Maximize results** command (`Ctrl+Alt+M`) for a larger debugger workspace.
The table, inspector, overview and raw output each scroll inside their own region.

PostgreSQL's parent costs, timings and block counters include children. They must not be
summed as query totals. Actual rows and total time in JSON are per-loop averages; the model
also provides their products with loops. Parallel products measure cumulative worker work,
not elapsed wall time. Comparable self cost/time is derived by subtracting serial children
only; ambiguous parallel, asynchronous, partial, init/subplan and repeated-loop cases remain
unavailable. The slowest-step overview excludes parallel worker totals. Operation statistics
explicitly report cumulative inclusive time, with overlapping parents and children.

Buffer hits and reads combine shared and local blocks (temporary tables can use local
buffers). Temporary read/written blocks stay separate. Temporary-operation counts identify
known materialization/memoization operations and disk sorts, rather than counting inclusive
temporary I/O again on ancestors. Missing values remain unavailable instead of becoming zero.

Findings are generic heuristics over normalized metrics, not automatic tuning advice:
hotspots require comparable serial self work of at least half an execution lasting 5 ms;
row gaps require a tenfold difference and at least 100 rows; most-filtered requires at least
100 discarded rows and nine discarded per returned row; large sequential scans require at
least 10,000 known visited rows (returned plus filtered, multiplied by loops), or estimated
rows when execution metrics are absent. Other findings identify disk sorts, at least 1,000
heap fetches or loops, missing planned workers, at least 100 reads exceeding cache hits, and
zero-loop steps. A never-executed step receives only that finding. Check the workload and
server measurements before changing indexes, statistics or settings.

## MySQL and MariaDB

Both engines' JSON lists **tables in join order** inside query blocks, not an operator tree.
The tree shows what the JSON names: query blocks, nested loops (one node over the tables it
joins; a single table is not a join), MariaDB's block nested loops, ordering, grouping, sorts,
temporary tables, materializations, unions and subqueries. Each table carries its access type
(full table scan, index lookup…), key, attached condition, index condition, and the possible
keys, key parts and `ref` in its engine detail. Estimated rows are rows examined per scan ×
filtered % (the rows a table passes on per scan, like PostgreSQL's estimate). A table's cost is
its own read and evaluation cost; the query block's is the query's. Tables are named by their
alias in the query, so relation counts count aliases.

MySQL's `EXPLAIN ANALYZE` tree (and a saved `FORMAT=TREE` or format version 2 JSON, which a
session with `explain_json_format_version = 2` returns) is an operator tree with inclusive
costs and, after ANALYZE, actual rows and time per loop, multiplied by loops for totals, as on
PostgreSQL. The step text (`Index lookup on c using PRIMARY (id=o.customer_id)`) is kept as
the engine detail; the node type, table, index and conditions are read from it without
guessing (a `<temporary>` table is not a relation). Self metrics subtract children only when
they ran as often as the parent, never under LIMIT, nested loops or subqueries. MySQL reports
no buffers, workers or sort memory, and prints no figures for a hash join's build step.

MariaDB's `ANALYZE FORMAT=JSON` adds loops (`r_loops`), rows read per loop (`r_rows`), the share
kept by the attached condition (`r_filtered`: the rest count as rows filtered), each table's
own time (`r_table_time_ms` + `r_other_time_ms`) and InnoDB page accesses (`r_engine_stats`:
accesses not read from disk are buffer hits, pages read from disk are reads; MariaDB omits zero
counters). Query blocks carry inclusive totals across loops; a sort's time, and a block join's
unpacking and condition time, are their own. A table has no inclusive time, so one with a
materialized child shows only its self time. Optimization time is the planning time. A
single-table UPDATE or DELETE reports no loops; its rows and times still mark it as measured.

## SQLite

`EXPLAIN QUERY PLAN` answers with one row per step (`id`, `parent`, `detail`). The core keeps
the rows as a JSON array, the one `sqlite3 -json` prints, and builds the tree under one
**Query plan** root, in preorder, refusing a row whose parent does not come before it. Steps
are in nested-loop order: each SCAN or SEARCH visits one table inside the steps before it, so
joins are not steps of their own. The step's text is kept whole as its engine detail; from it
come the step type (scan, rowid search, index or covering index search, automatic index,
temporary B-tree, subquery, materialization, compound query…), the table (named by its alias,
as SQLite writes it; the `SCAN TABLE t AS a` form of SQLite before 3.36 too), the index and the
condition it is searched by. A SCAN without an index is a sequential scan; a temporary B-tree
for ORDER BY counts as a sort. Text this model does not know becomes the step type as written.
SQLite reports no costs, row estimates or timings, so every metric is unavailable and there are
no findings.

SQLite on a server (ADR-018) gets the same plan: its `sqlite3` shell starts with
`.explain off`, so `EXPLAIN QUERY PLAN` comes back as rows in quote mode, not as the shell's
drawing. A saved copy of that drawing (`QUERY PLAN` / `|--SCAN o`) opens as the same tree.

## DuckDB

`EXPLAIN (FORMAT JSON)` is an operator tree: each operator's name (`HASH_JOIN`, `SEQ_SCAN`…),
its estimated cardinality, its table (`catalog.schema.table`), filters, join type, the
conditions of a hash join, and its other details (projections, aggregates, groups, order…) as
the engine detail. DuckDB reports no costs. `EXPLAIN (ANALYZE, FORMAT JSON)` executes the
statement (with the same warning and confirmations as on the other engines) and returns the
profiler's tree: the core drops the operators DuckDB wraps around the explained query
(`RESULT_COLLECTOR`, `EXPLAIN_ANALYZE`) and adds each operator's output rows and its own time.
Operators run once per query in parallel pipelines, so loops are 1, rows per loop are the
operator's rows, and an operator's time is CPU time summed over threads; a step's cumulative
time is its own time plus its children's. A scan's rows filtered are the rows it read minus
the rows it kept. Execution is DuckDB's total query time. The profiler output of older DuckDB
versions (`operator_type`, `operator_timing`, `operator_cardinality`, `latency`) opens too.

## Files and CLI

The file menu and palette expose **Open saved plan**. The desktop also accepts
`spelaion-desktop --plan path/to/plan.json` (or `plan.sqlplan`) or a plan file as its first path argument.
The CLI reads and analyzes a saved file locally, without configuration, database access or
execution:

```sh
spelaiondb plan path/to/plan.json --offset 0 --limit 100
spelaiondb plan path/to/plan.json --csv path/to/plan.csv
spelaiondb plan path/to/plan.sqlplan --offset 0 --limit 100
```

JSON pages include `summary`, `page.nodes` and `page.nextOffset`; continue with the returned
offset. CSV exports the full tree from the local file. Plans are recognized by their shape:
PostgreSQL, MySQL/MariaDB, SQLite or DuckDB JSON, MySQL's text tree, the `sqlite3` shell's
`QUERY PLAN` tree, or SQL Server Showplan XML. Saving a raw MySQL ANALYZE plan or an opened
`QUERY PLAN` tree writes a `.txt` file, and SQL Server writes `.sqlplan`; other plans use JSON.

## MCP

Existing `explain_query` JSON/TOON output and the user's format preference are preserved.
Set `structured: true` for PostgreSQL, MySQL, MariaDB, SQL Server, SQLite or DuckDB to receive the same core tree, metrics and findings:

```json
{"connection":"~/reporting","sql":"SELECT * FROM orders WHERE id=1","structured":true,"limit":50}
```

Continue with `connection`, `plan_id: summary.id`, and `offset: page.nextOffset`, omitting
`sql`. Pages read the cached document and never re-execute the query. Release it with
`release: true`. Handles are scoped to that connection and server process, and visibility
is checked on every call. Queries and cache reads/releases are audited, with cache accesses
explicitly identified as executing no database statement. MCP ANALYZE remains unavailable;
the existing classifier, native read-only transaction, timeout and row limits still apply.
A SQLite plan has a row per step: the agent's row limit bounds query results, not those rows,
which are read whole up to the 10,000-node bound.

## Ownership and bounds

The core owns parsing, findings, documents, sorting, CSV and file behavior. Tauri, CLI and
MCP are thin adapters. React receives only a summary and bounded pages, statistics or raw
text windows, never database result sets. Crossing Rust types are generated from the core.
Internal Arrow handles from execution are released after parsing or failure. Open documents
are retained until explicitly released; capacity errors never silently evict an active view.

Limits: 8 MiB input (before and after decoding), 10,000 nodes, depth 60, 100 nodes/stat groups per page, 32 KiB raw
windows, and 32 retained documents with a conservative 64 MiB accounting budget per store.
MCP lowers a page's node count to fit complete JSON within 60 KB. A single oversized node
produces a clear error; JSON is never sliced or reparsed by an adapter. File writes use a
unique sibling temporary file and atomic replacement; failures/cancellation preserve an
existing destination and clean up the temporary file.

New PostgreSQL requests include `VERBOSE TRUE`, so relations and indexes retain schema
identity. Bitmap index nodes inherit the namespace of their bitmap heap access when needed.
Saved non-verbose files can omit schemas; their available names are preserved without
guessing, and relation/index counts can combine homonyms whose namespaces are unavailable.

Real fixtures, exact SQL and provenance are in
[`postgres`](../crates/spelaion-core/tests/fixtures/plans/postgres/README.md),
[`mysql`](../crates/spelaion-core/tests/fixtures/plans/mysql/README.md),
[`mariadb`](../crates/spelaion-core/tests/fixtures/plans/mariadb/README.md) and
[`sqlserver`](../crates/spelaion-core/tests/fixtures/plans/sqlserver/README.md) under
`crates/spelaion-core/tests/fixtures/plans`. SQLite and DuckDB are tested against the real
engines (`crates/spelaion-core/tests/plans_sqlite_duckdb.rs`; SQLite on a server with
`SPELAION_TEST_SSH`). The view includes each engine's capability note.
