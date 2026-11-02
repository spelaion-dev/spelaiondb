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

## Alternatives
Monaco (heavier), AG Grid (DOM, more editing features, heavier).

## Revisit if
Glide Data Grid does not perform on WebKitGTK: our own canvas grid.
