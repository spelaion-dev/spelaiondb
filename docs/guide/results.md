# Working with results

Select cells in the result grid by clicking and dragging, with Shift and the arrow keys, by
clicking row numbers (whole rows) or with Ctrl+A. **Ctrl+C** copies the selection as
tab-separated text for spreadsheets and **Ctrl+Shift+C** adds the column names. The
right-click menu also copies as CSV, JSON (numbers stay numbers, JSON columns are embedded)
or a single SQL `INSERT` with all selected rows and the engine's quoting, as a Markdown table or as an SQL
`IN` list (`(1, 2, 3)`, repeated values once). The core formats the rows from the
typed data; up to 100,000 rows can be copied at once.

**Export ▾** writes the whole result to a file (CSV with a byte order mark so Excel reads
accents, TSV, JSON, SQL `INSERT` or a Markdown table). The core writes it in chunks to a temporary file that is
renamed at the end, so it works with millions of rows and never leaves half-written files.

Drag the edge of a column header to resize the column; double-click the edge to fit it to
its name and the visible values.

Clicking a column header sorts the loaded rows ascending, then descending, then back to the
server's order (also in the right-click menu). The core sorts by the typed values (numbers and
decimals by value, dates by date) with NULLs last, and copies and exports follow that order.
**Filter rows** above the grid keeps the rows where some cell contains the text (ignoring
case), also filtered in the core: the status line shows "15 of 50,000 rows", sorting works on
what is left, and copies and exports take only those rows. **Esc** clears it.

Column filters work like SQLyog's: right-click a cell, **Filter ▸** offers `= 'value'`,
`<> 'value'`, `>`, `<`, `LIKE '%value%'`, `NOT LIKE`, `IS NULL` and `IS NOT NULL` for that cell,
plus **Custom filter…** (any column, operator and value) and **Reset filters**. Conditions add
up (AND), show above the grid with a × each, and follow SQL: a NULL only matches `IS NULL`.
Numbers compare by value and text ignores case. They run in the core on the loaded rows, so
they also work where a `WHERE` cannot go, such as `SHOW FULL PROCESSLIST` filtered by
`Command <> 'Sleep'`.

**Enter** or a double-click opens the cell viewer beside the grid with the full value of the
active cell: JSON pretty-printed, binary values as a hex dump and long text with its line
breaks. **Row** switches to every column of the active row, one under the other (click a
field to see that value in full). The arrow keys move through cells while it stays open;
**Esc** closes it.
