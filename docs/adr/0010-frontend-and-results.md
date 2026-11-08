# ADR-010: Frontend and large results

- **Status:** Conditional on phase 0 (experiments 1 and 3)
- **Date:** 2026-11-02

## Decision
- **React + TypeScript + Vite**, Tailwind + Radix (shadcn/ui), themes with CSS variables,
  panels with `dockview`. Chosen for the agent's reliability; the heavy parts (grid,
  editor) do not go through React.
- **Editor:** CodeMirror 6 with `@codemirror/lang-sql`.
- **Autocompletion** in the frontend: a tokenizer tolerant of incomplete SQL + scope
  heuristics (FROM/JOIN, aliases, CTEs) over the schema cache.
- **Canvas grid** (Glide Data Grid) behind our own interface, so it can be replaced by our
  own canvas grid.
- **The result lives in the core, not in the frontend:**
  - the core receives streamed rows, converts them to Arrow batches, keeps them in memory
    up to a budget and then on disk;
  - the UI requests windows of rows while scrolling and receives them as binary (Arrow
    IPC) over Tauri IPC, without JSON;
  - sorting and filtering what is loaded is done by the core.
- By default the first few thousand rows are fetched; "load more / all" is explicit.
- UI state with Zustand; results never in React state.
- TS types generated from Rust.

## Phase 0 evidence (2026-11-02, `spikes/grid-stream`)
- Windows/WebView2: 1 M rows (synthetic and from PostgreSQL) scrolling at 164 fps with no
  slow frames; first PostgreSQL row on screen after 33 ms; JS heap ~20 MB; a window of
  1,000 rows in Arrow IPC in ~3 ms. The "result in the backend" design works.
- **Glide Data Grid is abandoned** (last release Feb. 2024, no React 19). It served for
  measuring, but the product will have its own canvas grid behind the same interface.
- A new result mounts a new grid (otherwise it inherits the scroll position).
- WebKitGTK on WSLg without a GPU: 50–55 fps, p95 21 ms, first PostgreSQL row after 64 ms.
  Real Linux with a GPU is still missing.

## App implementation (2026-11-02)
- Our own canvas grid (`apps/desktop/src/components/ResultGrid.tsx`): native scrolling with
  a spacer, fixed header and row numbers, right-aligned numeric cells, dimmed NULL, cell
  selection and copy. Blocks of 500 rows; at most 80 in memory.
- `fetch_window` sends each window as Arrow IPC with the cells **already converted to text
  by the core** (`Value`, identical to `psql`): the UI does not reimplement formats for
  dates, decimals or intervals. Typed data stays in the core for sorting and editing.
- CodeMirror 6 editor with the engine's dialect, table autocompletion and the error
  underlined at its position (the core gives UTF-8 bytes; the UI converts to UTF-16
  indexes).
- Tested end to end with a smoke test that drives the real app over CDP.

## Grid quick wins (2026-11-07, SDB-5)
- **Find (Ctrl+F) runs in the core** (`find.rs`): it reads every cell of the current view (after
  the sort and the filters) as the grid shows it, keeps the matches in the stored result (at
  most 1,000,000) and serves them in windows; the grid highlights only the rows on screen and
  steps with `find_step`. A sort, a filter or appended rows drop the matches and the grid
  searches again; a newer search stops an older one. React never holds rows.
- **"Filter by this value"** builds a `filter.rs` column filter (new: contains / does not
  contain, taken literally) on query results. In a **table data tab** it goes to the server
  instead: the core writes the condition from the typed cell (quoted column, the engine's
  literal) and adds it to the tab's WHERE, which `table_data.rs` checks as always. Filtering
  only the loaded page of a big table would hide rows that match on other pages.
- **Copies** are written by the core: HTML tables, column names (always quoted, or qualified
  like the explorer's) and a column's distinct values as an `IN` list, refused beyond 10,000
  values rather than cut.
- Hidden and frozen columns are view state of the tab (in memory); display settings (zebra
  rows, NULL text, boolean checkboxes) are personal settings in the store.

## Numbers about the selection (2026-11-08, SDB-12)
- **The calc panel computes in the core** (`selection_stats.rs`): the grid sends the selection's
  bounds (rows of the current view and the columns it shows, the `CellRange` copies already
  use) and gets back count, distinct, NULLs, sum, average, median, minimum, maximum and mode.
  No cell value crosses to React. Sum, average and median take the numeric cells only; integers
  and decimals add up exactly (a float makes the result a float); minimum and maximum compare
  by type and are absent when the selection mixes kinds that do not compare. Distinct values
  are counted up to 1,000,000. A newer calculation on the same result, or Cancel, stops one
  still running, as a newer search stops Ctrl+F.
- **Column statistics ask the server** (`column_stats.rs`), because a table data tab holds one
  page: rows, distinct, NULLs, minimum, maximum and the ten most frequent values of the table
  with the tab's filter. Every statement is written by the core from the catalog's names, the
  filter is checked like a page's, and each runs as a single read inside the engine's
  read-only transaction (ADR-034's barrier) in a session of its own, closed afterwards. Past
  100,000 rows the numbers describe the first 100,000 rows the server returns and are marked
  "Sampled": `TABLESAMPLE` does not exist in MySQL or SQLite, and the same rule everywhere is
  easier to read. Each number has its own statement, so one the engine refuses for the type
  (PostgreSQL's `json`) leaves the others. A most frequent value carries the core's
  "filter by this value" condition, which the popover adds to the tab's filter.

## Alternatives
Monaco (heavier), AG Grid (DOM, more editing features, heavier).

## Revisit if
Glide Data Grid does not perform on WebKitGTK: our own canvas grid.
