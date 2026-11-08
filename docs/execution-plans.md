# Execution plans

In a PostgreSQL query tab, choose **Explain query** in the action column, editor menu or
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

Read-only connections permit estimated read plans only. ANALYZE is refused in the core,
including when a caller bypasses the disabled UI control. The policy comes from the actual
resolved open connection. PostgreSQL's driver `read_only` flag alone is insufficient.

## Reading a plan

- **Overview** shows cost, planning/execution time, comparable self work, row estimate gaps,
  operation counts, depth, relations and indexes. Findings link to their steps.
- **Tree & table** sorts siblings while keeping parents before their children. All optional
  metrics are columns; collapse branches or filter to steps with findings. CSV exports the
  current table view, including all metric columns, using formula protection for text and
  headers by default.
- **Graph** presents the same bounded node page and selectable inspector. Edges connect
  visible parents and children; a parent outside the page has a navigation link.
- **Raw output** displays the original JSON in UTF-8 windows. **Save raw plan** writes the
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

## Files and CLI

The file menu and palette expose **Open saved plan**. The desktop also accepts
`spelaion-desktop --plan path/to/plan.json` or a plan file as its first path argument.
The CLI reads and analyzes a saved file locally, without configuration, database access or
execution:

```sh
spelaiondb plan path/to/plan.json --offset 0 --limit 100
spelaiondb plan path/to/plan.json --csv path/to/plan.csv
```

JSON pages include `summary`, `page.nodes` and `page.nextOffset`; continue with the returned
offset. CSV exports the full tree from the local file.

## MCP

Existing `explain_query` JSON/TOON output and the user's format preference are preserved.
Set `structured: true` for PostgreSQL to receive the same core tree, metrics and findings:

```json
{"connection":"~/reporting","sql":"SELECT * FROM orders WHERE id=1","structured":true,"limit":50}
```

Continue with `connection`, `plan_id: summary.id`, and `offset: page.nextOffset`, omitting
`sql`. Pages read the cached document and never re-execute the query. Release it with
`release: true`. Handles are scoped to that connection and server process, and visibility
is checked on every call. Queries and cache reads/releases are audited, with cache accesses
explicitly identified as executing no database statement. MCP ANALYZE remains unavailable;
the existing classifier, native read-only transaction, timeout and row limits still apply.

## Ownership and bounds

The core owns parsing, findings, documents, sorting, CSV and file behavior. Tauri, CLI and
MCP are thin adapters. React receives only a summary and bounded pages, statistics or raw
text windows, never database result sets. Crossing Rust types are generated from the core.
Internal Arrow handles from execution are released after parsing or failure. Open documents
are retained until explicitly released; capacity errors never silently evict an active view.

Limits: 8 MiB UTF-8 input, 10,000 nodes, depth 60, 100 nodes/stat groups per page, 32 KiB raw
windows, and 32 retained documents with a conservative 64 MiB accounting budget per store.
MCP lowers a page's node count to fit complete JSON within 60 KB. A single oversized node
produces a clear error; JSON is never sliced or reparsed by an adapter. File writes use a
unique sibling temporary file and atomic replacement; failures/cancellation preserve an
existing destination and clean up the temporary file.

New PostgreSQL requests include `VERBOSE TRUE`, so relations and indexes retain schema
identity. Bitmap index nodes inherit the namespace of their bitmap heap access when needed.
Saved non-verbose files can omit schemas; their available names are preserved without
guessing, and relation/index counts can combine homonyms whose namespaces are unavailable.

Real PostgreSQL fixtures, exact SQL and provenance are in
[`crates/spelaion-core/tests/fixtures/plans/postgres`](../crates/spelaion-core/tests/fixtures/plans/postgres/README.md).
Other engines can build on the common model in their own features; this change implements
PostgreSQL only. The view includes an engine capability note.
