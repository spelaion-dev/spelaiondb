# ADR-046: ER diagrams: laid out in the core from the explorer's catalog, drawn as plain SVG

- **Status:** Accepted
- **Date:** 2026-11-10
- **Builds on:** ADR-024 (the explorer's catalog), ADR-040 (plain SVG, colors from the theme),
  ADR-041 (what crosses the driver contract is owned data)

## Context
An unknown schema is understood much faster from a picture of its tables and the keys between
them. A diagram reads the same catalog the explorer does, can hold hundreds of tables, and must
stay usable while the user pans and zooms around it. Other tools (Mermaid in documentation,
dbdiagram.io) read their own text formats, so the picture should also leave the app as text.

## Decision
1. **The model comes from the catalog the explorer reads** (`spelaion-core::er`): a schema's
   tables from `list_objects` (or `list_objects_matching` with the explorer's table filter,
   ADR-025), chosen tables, or one table with every table it points to and that points to it
   (`describe_table` and `list_referencing_keys`). Each table is described once, in order, on the
   tab's own session; one that cannot be described is counted and left out, and the read goes
   on. Views are left out (they have no keys). At most 500 tables are read
   (`MAX_DIAGRAM_TABLES`); past that the rest, by name, are counted and the diagram says so.
   No new driver call: the model is owned data built from what the contract already returns.
2. **Cardinality from the keys themselves:** the referenced end is "exactly one" when every key
   column is `NOT NULL` (or in the primary key), else "zero or one"; the referencing end is
   "zero or one" when the key's columns are the table's primary key or a unique constraint,
   else "zero or many". A key whose columns are all in the referencing table's primary key is
   identifying. Crow's foot notation draws them.
3. **The layout is the core's, computed once per model and display options**
   (`spelaion-core::er_layout`): a layered layout where referenced tables sit left of the
   tables that reference them (cycles broken where they close), a few rounds of crossing
   reduction by neighbours' average position, tall columns wrapped, groups of joined tables
   packed largest first and tables with no keys after them. Box sizes follow what they show in
   the UI's monospace font, from the character width the UI measures and sends. Each relation
   leaves and enters at the row of its column, with a stub at each end for the marks. The
   desktop keeps the model by tab between calls, so changing a display option lays it out again
   without reading the catalog; panning and zooming are one SVG transform and never ask for a
   layout. 200 generated tables with 256 keys lay out in about 8 ms in a debug build
   (`tests/er_diagram.rs`); in the release app a new layout of 203 tables is on screen about
   200 ms after the click, drawing included (`e2e/smoke.mjs`).
4. **Plain SVG, no diagram library**, like charts (ADR-040) and the map (ADR-045): boxes, text
   and curves in a few hundred lines of React. A layout library (ELK, dagre) would add 0.5–8 MB
   of JavaScript and put the layout in the UI; the core's is small, deterministic and tested.
   Colors are the theme's tokens for both themes; the primary key mark is also written ("PK"),
   never shown by color alone.
5. **Display options:** all columns, keys only (primary, foreign and unique key columns) or
   table names only; column types on or off; comments on or off. They are kept while the tab is
   open, not saved.
6. **Exports:** PNG and SVG are drawn by the UI from the whole layout (styles written on the
   elements) and written by the core's image check (`save_chart_image`); Mermaid (`erDiagram`)
   and DBML are written by the core from the whole model (`mermaid`, `dbml`), whatever the
   display options, and saved whole (`save_diagram_text`). Characters a format cannot hold are
   replaced, never dropped silently (`er/text.rs` says which). Both texts must parse with their
   own tools: `scripts/er-text.test.mjs` runs Mermaid's and DBML's parsers (`mermaid`,
   `@dbml/core`, development dependencies only, never in the app's bundle) on fixtures the core
   writes from an awkward schema; `npm run build`, and so CI, runs it, and a Rust test fails
   when the fixtures no longer match what the core writes.
7. **Where it lives:** "ER diagram" on a schema's explorer menu; on a table's, the diagram of its
   related tables, or of the tables chosen with Ctrl+click; the palette's "ER diagram of the
   current table or schema" command and a table's "ER diagram of related tables" action; and,
   in a diagram, a table's menu focuses on it in a new tab. Each diagram is a tab of its own.

## Consequences
- Reading a large schema through a slow tunnel takes one `describe_table` round trip per table;
  the tab shows how many are read. A batched catalog read is a later change of the driver
  contract.
- Saved diagrams (manual positions, notes, colors) are not part of this: when they come they
  are local data or project files, and the layout here is their starting point.

## Alternatives
- **A diagram library in the UI (React Flow, ELK, dagre, Mermaid's renderer):** more polished
  routing, but the layout would live in React, its styling must be overridden to follow the
  tokens, and the bundle grows by megabytes.
- **Rendering in the core (SVG or PNG written in Rust):** one renderer for screen and files,
  but a second drawing path for hover and selection, and PNG would need a rasteriser crate.
- **A force-directed layout:** fits any graph, but it is not deterministic without care, needs
  many iterations for 200 tables, and hides the direction of the keys that a layered layout
  shows.
