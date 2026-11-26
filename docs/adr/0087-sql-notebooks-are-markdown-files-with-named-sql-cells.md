# ADR-087: SQL notebooks are Markdown files with named SQL cells; a reference is a CTE of the cell's SQL; results are never saved in the file

- **Status:** Accepted
- **Date:** 2026-11-26
- **Builds on:** ADR-010 (results live in the core), ADR-022 (what a repository may bring),
  ADR-028 (nothing is re-run), ADR-031 (transactions), ADR-035 (script runs), ADR-036 (SQL
  files and kept tabs), ADR-040 (charts), ADR-051 (column masking), ADR-053 (read-only
  connections), ADR-065 (one lexical scanner per dialect)

## Context
An investigation is a few queries and the reasoning between them: "these customers stopped
ordering in March; here is what they bought before; here is what changed". Today it lives in
an editor tab with comments, or in a document someone pastes results into. A notebook keeps
the text, the queries and their results together, runs them top to bottom again tomorrow, and
can be shared with the team through the same git repository as `.spelaion/`.

Three questions decide whether a notebook can be shared safely and reviewed like code:

- **The file format.** It must be stable (a file written today opens in a year), easy to diff
  and to merge in git, and readable without SpelaionDB: a reviewer on GitHub should see the
  investigation, not a JSON blob.
- **How a cell uses another's result.** "The customers of cell 1, joined to orders" is the
  point of a notebook. The result of cell 1 lives in the core (ADR-010), but the database
  that runs cell 2 never saw it.
- **Whether results are written into the file.** Jupyter keeps outputs in the notebook, and
  that is how data ends up in public repositories. A result can hold customers' data; a
  connection may mask columns (ADR-051) precisely because some people should not see them;
  a Production connection's data is the most sensitive of all.

## Decision

### The file
1. **A notebook is a Markdown file** (`.md`, UTF-8 when the app writes it), read and written
   by the core (`spelaion-core`'s `notebook` module) through ADR-036's file rules: the path the
   user picked, at most 16 MiB, the encodings and line ending it was read with, atomic writes,
   and the content hash that notices a change on disk. GitHub, GitLab and any editor show it
   as a document; a diff shows the text and the SQL that changed, line by line.
2. **It starts with a front matter block that says it is a notebook**, version 1:

   ```text
   ---
   spelaion-notebook: 1
   connection: prod/orders
   ---
   ```

   The first line is exactly `---`; then `key: value` lines; then `---`. `spelaion-notebook`
   is required and is the format's version: a higher number is refused with "written by a
   newer SpelaionDB", never read in part. `connection` is optional: the id of the connection
   it was last run on, a suggestion the app offers when it opens (see point 11), never a
   connection opened by reading the file. Any other key, a repeated key or a line that is not
   `key: value` is refused with its line number. A Markdown file without this block is not a
   notebook and opens nothing.
3. **A SQL cell is a fenced code block whose info string is `sql cell=<name>`:**

   ````text
   ```sql cell=lost_customers
   SELECT customer_id, max(ordered_at) AS last_order
   FROM orders GROUP BY customer_id
   HAVING max(ordered_at) < DATE '2026-03-01'
   ```
   ````

   The fence is three backticks or more, alone at the start of a line, and the cell ends at a
   line of the same backticks alone. The text in between is the cell's SQL, kept byte for byte
   (line breaks as `\n` in memory, written with the file's line ending). The writer uses a
   fence one backtick longer than the longest run of backticks that starts a line of the SQL
   (three at least), so any SQL round-trips.
4. **Everything else is Markdown cells.** The text between two SQL cells is one Markdown cell;
   two Markdown cells in a row are separated by a line `<!-- cell -->` alone (an HTML comment,
   which the Markdown viewers hide). The writer puts one blank line between parts and the
   reader drops the blank lines around a Markdown cell, so its leading and trailing blank
   lines are not kept, and an empty Markdown cell is not saved. A Markdown cell may not hold a
   line that would read as a SQL cell's opening fence or as `<!-- cell -->`: saving refuses it
   and names the cell, rather than writing a file that reads back differently.
5. **Cell names** are what references use and what the HTML export and the summary show: a
   letter or `_`, then letters, digits or `_`, at most 63 characters, unique in the notebook
   without regard to case. A new cell gets the first free `cell_<n>`; the user may rename it,
   and the core refuses a name that is taken or not valid. A notebook holds at most 500 cells.
6. **Round trip is a contract:** reading what the writer wrote gives the same cells, and
   writing what the reader read gives the same bytes. The core's tests hold it, for SQL with
   backticks, blank lines, `\r\n` files and every encoding of ADR-036.

### References between cells
7. **A cell names another as `{{name}}`**, written where a table name goes:
   `SELECT * FROM {{lost_customers}} c JOIN orders o USING (customer_id)`. The core finds
   references only in the cell's code, outside strings, quoted names and comments, with the
   connection dialect's own scanner (ADR-065), so `'{{x}}'` in a string is text.
8. **A reference is a CTE of the referenced cell's SQL, run again by the database** as part of
   the cell that names it. Before the guard sees anything, the core expands the cell into one
   statement: `WITH "lost_customers" AS (<its SQL>) SELECT * FROM "lost_customers" c …`, the
   name quoted by the dialect, the referenced cells first in the order they are needed (a
   cell that references another brings that one along). When the cell already starts with
   `WITH` (or `WITH RECURSIVE`), the references go first in its own list. It is the referenced
   cell's **SQL** that runs, not the rows it showed: if the data changed since, the reference
   sees today's rows. This is said in the cell, next to its references.
9. **What may be referenced is a read, and only one:** the referenced cell must hold exactly
   one statement, which the core's read classifier (`classify_read_only`, the barrier of
   read-only connections and agents) reads as a query, so a reference can never repeat a
   write (PostgreSQL's `WITH x AS (DELETE … RETURNING …)` included). A trailing `;` is
   dropped. A name that does not exist, a cell that references itself, a cycle, a Markdown
   cell, more than 50 cells in one expansion or an expansion over 16 MiB are refused with the
   reason, and nothing runs. References need a SQL engine: an engine whose language is not SQL
   (ADR-056) refuses them, and its cells still run on their own. A referenced cell with its own
   `WITH` works where the engine allows a `WITH` inside a CTE (not SQL Server); where it does
   not, the engine's error is shown.
10. **The expanded statement is what runs, and what every check sees:** it goes through the
    same `check_run` as the editor (the read-only barrier of ADR-053, then the guard's
    confirmations of ADR-029), its risks are asked before it runs, and the history records
    what ran. The page can show it ("Show the SQL that runs") before running.

### Running
11. **Opening a notebook never runs it, and never connects.** A notebook opens as a tab with
    its cells and no result. The front matter's connection is selected for the tab when one
    open folder or the personal layer has a connection with that id, as a restored tab's
    would be (ADR-036 point 5); otherwise the tab has none, and a note says the connection was
    not found. Connecting is the user's click, through the ordinary path (passwords, trust).
12. **A notebook tab has one session, like an editor tab:** auto-commit or manual commit and
    the transaction controls of ADR-031, the tab's run options of ADR-035, and its read-only
    mode. A cell is a script run on that session (`run_script`'s path): it may hold several
    statements, its error mode is the tab's, Cancel stops it.
13. **Each cell keeps its own results in the core,** in the result tabs of ADR-010 and SDB-378
    under a key of its own, shown below the cell as a grid of a fixed height, with the chart of
    ADR-040 one click away. Running the cell again replaces them; deleting the cell or closing
    the notebook frees them. The page never holds rows: grids ask for windows, charts for
    points, as anywhere else.
14. **Run all** runs the SQL cells from the top, one after another, skipping Markdown cells. It
    stops at the first cell that fails, that is refused, whose confirmation is declined or that
    is cancelled, and then says what happened: how many cells ran, the cell that stopped it and
    why, how many were not run, and the time it took. A notebook is not connected by Run all
    unless the user clicked it on a disconnected tab, as Run does in the editor.

### Results are not saved in the file
15. **A notebook file holds text and SQL, never results.** There is no setting to write them:
    a result committed to git stays in its history after it is deleted, and a notebook is
    meant to be shared. What would have been a snapshot is the HTML export (point 17), a file
    the user writes on purpose, outside the notebook.
16. **Kept tabs** (ADR-036 point 3): an open notebook is kept across a restart like a file tab,
    with its path and, when it has changes not saved, its document as text (`open_tabs`, a new
    `notebook` column); never results, a session or a transaction. It comes back disconnected,
    with no results, as point 11 says, and quitting does not ask about it.

### Export as HTML
17. **Export as HTML writes one self-contained file** (by the core, atomically, to the path the
    user picked): the title, the connection's name and when it was written, each Markdown cell
    as HTML, each SQL cell's text, and each of its results as a table of at most 1,000 rows,
    saying how many there were. The cells' values are the text the grid shows, read from the
    stored results, so a masked column (ADR-051) stays masked. A cell whose chart is shown
    carries it as an image: the page sends the chart's SVG, which the core checks with the
    rules of `save_chart_image` (ADR-040: no script, no embedded HTML) and embeds as a
    `data:image/svg+xml` image, so nothing in it can run. Cells not run say so. Every text is
    escaped; the file has no script, loads nothing from the network, and declares
    `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; img-src data:`.

### Markdown is shown as text
18. **The core reads Markdown cells into a small block model** and both the page and the HTML
    export draw that model: headings, paragraphs, block quotes, bulleted and numbered lists,
    fenced code, thematic breaks and tables; inside them strong, emphasis, code and links.
    Anything else is shown as the text it is written with; raw HTML is shown as its text, never
    as markup.
    A link shows its text and its address; neither the page nor the export follows it
    (`href` is not written), and an image shows its alternative text and address and is never
    fetched. A notebook from a repository is text written by someone else, as a `.sql` file is
    (ADR-036 point 8): reading one needs no trust and can do nothing.

## Consequences
- A notebook from git is reviewed as text, and opening one is as safe as opening a `.sql`
  file: nothing runs and nothing connects until the user clicks.
- A reference re-runs the referenced query: an expensive cell referenced by three cells runs
  four times in Run all. The cell says it; materialising a result is something a user can still
  write as SQL (`CREATE TEMP TABLE … AS`), in a cell of its own, through the guard.
- A shared notebook shows no data until it runs. Someone without access to the connection
  sees the reasoning and the SQL only, which is the point of point 15; to show results to
  someone, the export is a deliberate act.
- The format is Markdown with three rules of our own (front matter, the cell fence, the cell
  separator); people can write one by hand in any editor, and a mistake is refused with its line.

## Alternatives
- **Jupyter's `.ipynb` (JSON, outputs inside):** tools exist for it, but its diffs are JSON,
  every edit rewrites metadata, and outputs in the file is what point 15 rules out.
- **A TOML document with an array of cells:** the configuration is TOML and a parser is at
  hand, but GitHub shows it as code, multi-line SQL needs escaping rules of its own, and the
  Markdown between queries is the part people read.
- **A `.sql` file with cell markers in comments (`-- %% name`):** it runs as a plain script
  elsewhere, but the Markdown would be comment lines, and running it whole would ignore the
  references.
- **Snapshots in the file, off by default, bounded, never for masked columns or Production:**
  the card allowed it. Every one of those conditions is a check that a later change can break
  while the file is already in a repository's history; a separate export the user writes on
  purpose leaks nothing by default. Revisit below.
- **References through the core: the referenced cell's stored rows sent back to the database
  as a temporary table or a `VALUES` list:** it would use exactly what was shown, but it writes
  (a temporary table needs write rights, and read-only connections refuse it), a `VALUES` list
  of a million rows is not a statement any engine wants, and the types would be the grid's text.
- **Implicit references (a cell's name used as a table name):** shorter, but a real table with
  the same name would silently become the cell, and a reader of the SQL could not tell.
- **A Markdown library (`pulldown-cmark`, `comrak`):** complete CommonMark, but a dependency
  whose raw-HTML handling must be filtered at every release; the subset of point 18 is what
  notes in an investigation use, and it is drawn the same in the app and in the export.

## Revisit if
- People ask to keep results with the notebook: then a snapshot is a separate file next to it
  (`<name>.results.html`, the export), never inside the notebook, and the conditions the card
  named (off by default, bounded, never masked or Production) apply to writing it.
- A cell per connection is asked for: the front matter's `connection` becomes the default and a
  cell may name its own, with references only between cells of the same connection.
- Agents should run notebooks: then the MCP server reads them with point 9's barrier and the
  agents' own limits, as a separate decision.
