# ADR-040: Charts from a result: points prepared in the core, drawn as plain SVG

- **Status:** Accepted (implemented)
- **Date:** 2026-11-09
- **Builds on:** ADR-010 (results live in the core), ADR-029 (a fixed palette per theme)

## Context
A quick look at a trend (sales per month, rows per status) should not need another tool. A chart
is drawn from a result that may hold a million rows, and ADR-010 keeps rows in the core: the UI
asks for windows and never holds the whole result. A chart must not become the way rows reach
React, and a chart over many rows must not quietly show only some of them.

## Decision
1. **The core prepares the points** (`spelaion-core::chart`, `StoredResult::chart`). The UI names
   the chart's kind (bar, line, pie), its X column, its Y columns (numbers only, at most eight)
   and an aggregate (sum, average, minimum, maximum, count); the core reads the result's current
   view (the grid's sort and filters, as the calc panel and copies do) and answers with labels,
   one value per label and series, and what it did to get them. The `result_chart` command
   carries points, never rows, as `bindings.ts` shows.
2. **At most 2,000 points, whatever the rows** (`MAX_CHART_POINTS`). Rows that share an X value
   are combined with the aggregate, as `GROUP BY x` would. A continuous X (numbers, dates, times,
   timestamps) is drawn in order; with more than 2,000 values its range is split into 2,000
   equal ranges, each labelled with its smallest value. Any other X keeps the order its values
   first appear in (so the grid's sort orders the bars); past 2,000 values the rest are combined
   in a last "Other" point. A pie has one series, adds up (sum or count) and keeps its eight
   largest positive slices, the last one "Other"; it ranks the first 100,000 values.
3. **The chart says what it did.** The answer carries the reduction (one point per row, grouped,
   equal ranges, or "Other"), the rows read, the rows left out (an X that is NULL or infinite on
   a continuous axis), the Y cells with no number and the pie values with no slice; a line under
   the controls says each of them, and whether the rows are only a table page or the loaded part
   of a query. Aggregating rather than sampling means every row counts, and the same rule holds
   for every engine.
4. **Plain SVG, no chart library.** Bars, lines and pies are a few hundred lines of React over
   SVG, with axes, a legend for two series or more, a hover and keyboard tooltip and an
   accessible name. A library would add bundle weight and its own styling for three chart kinds.
5. **Colors come from the theme** like ADR-029's tones: `--chart-1` … `--chart-8` and
   `--chart-other`, defined for the light and the dark theme in `styles.css`. Series take them in
   a fixed order; the palette is a categorical one checked for color vision deficiencies (adjacent
   pairs distinguishable under protanopia, deuteranopia and tritanopia) and for contrast on each
   theme's panel. Three light colors are under 3:1 on white, so values are never shown by color
   alone: the legend, the pie's labels and the tooltip carry them, and the grid is one click away.
6. **Save image…** writes the chart on screen as SVG (colors and fonts written on the elements,
   on the chart's background) or as PNG (the SVG drawn on a canvas at twice its size). The file
   is written by the core (`save_chart_image`), which only accepts a PNG (by its signature) or an
   SVG with no script or embedded HTML, at most 32 MB, replacing the file whole.
7. **Where it lives:** a chart button in the results toolbar switches the active result between
   the grid and its chart (the grid stays mounted, with its selection); "Chart selected columns"
   in the grid's cell menu opens it with the first selected column as X and the numeric ones
   after it as values. Settings are kept per result while the tab is open, not saved.

## Consequences
- A chart over a big result reads every row of the view in the core, twice for a continuous X
  (its range first). A million rows take well under a second in release builds; a newer chart or
  a cancel stops one still running, like the calc panel.
- Charts of a table data tab show its page only, and say so; charting a whole table is a query.

## Alternatives
- **A chart library (Recharts, Chart.js, ECharts):** more chart kinds and polish, but 100–900 kB
  more JavaScript and styling that must be overridden to follow the tokens; worth revisiting if
  people ask for scatter plots, stacking or zoom.
- **Sampling rows (every n-th, LTTB):** cheaper, but a sampled bar or pie misstates totals, and a
  chart that drops rows is harder to explain than one that combines them.
- **Rendering in the core (an SVG writer in Rust):** one renderer for the screen and the files,
  but a second rendering path for hover and keyboard, and PNG would need a rasteriser crate.
