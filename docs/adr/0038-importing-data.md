# ADR-038: Importing data into a table: values bound, statements shown first, whole batches

- **Status:** Accepted (implemented)
- **Date:** 2026-11-08
- **Builds on:** ADR-005 (driver model), ADR-014 (table edits), ADR-026 (object actions),
  ADR-029 (connection types), ADR-031 (transactions), ADR-034 (streaming exports)

## Context
Loading a CSV or a table pasted from a spreadsheet into a database is a weekly task, and today it
needs another tool. An import writes many rows at once from a file the app did not make: its
values may contain quotes and SQL, its header names become identifiers, and it may be large
(a million rows). The rules that keep writes safe elsewhere must hold: the user sees what will
run, read-only connections stay read-only, Production asks first, nothing is half-done without
saying so, and values never become SQL text.

## Decision
1. **The core does all of it** (`spelaion-core::import`): it reads the source, detects the types,
   writes every statement, checks the request against the table, decides what needs a
   confirmation, and loads the rows. The app sends the user's choices (the source, the options,
   the target, where each column goes) and shows what the core answers; it never sends SQL.
2. **Sources:** a file or pasted text, as CSV, TSV, a JSON array (of objects, keys in the order
   they first appear, or of arrays) or a Markdown table. The format comes from the file's
   extension or the text itself, the CSV separator from its first line (`;` where it has no
   commas). Options: encoding (detected from a byte order mark, else UTF-8 when the start is
   valid UTF-8, else Windows-1252), separator, quote and escape characters (a doubled quote by
   default), header row, NULL text, empty as NULL, trim, date and timestamp formats (`strftime`)
   and the time zone of timestamps without one. XLSX is left for later.
3. **The source is untrusted input.** CSV and TSV are streamed a line at a time; a record (a
   quoted value spanning lines included) may take at most 16 MiB, so a quote that never closes
   or an endless line stops with an error instead of filling memory. At most 2,048 columns. JSON,
   Markdown and files that are not UTF-8 are read whole, up to 512 MiB. Header names lose control
   characters, keep at most 63 bytes and are made unique. Names are always quoted with the
   engine's quoting; a column type the user types (it goes into `CREATE TABLE` as written) may
   only be words, digits and one list of sizes in parentheses, so it cannot end the statement.
4. **Types are detected from a sample** (1,000 rows by default): integer (no leading zeros, fits
   64 bits), decimal, float (with an exponent), boolean (`true`/`false`), date, timestamp, and
   timestamp with time zone. Integers widen to decimals, numbers to floats, dates to timestamps;
   **any other mix is text**, as is a column with no values. Each engine gets its types (`BIGINT`,
   `NUMERIC` or `DECIMAL(p,s)` with room for the digits seen, `BOOLEAN`/`BIT`, `DATETIME2`,
   `NVARCHAR(MAX)`…), which the user may change before the table is created.
5. **Values are never SQL text.** A new driver method, `Connection::insert_rows`, takes the
   table, its columns and rows of text or NULL, and each engine sends them through its own path:
   - PostgreSQL: `COPY … FROM STDIN` in text format, values escaped as copy data;
   - MySQL and MariaDB: prepared multi-row `INSERT … VALUES (?, …)` (up to 1,000 rows or about
     4 MiB each). `LOAD DATA LOCAL INFILE` is not used: it needs `local_infile` on the server and
     the client, off by default, and it lets a server ask the client for any file;
   - SQL Server: a TDS bulk load (the protocol's bulk path) into a session temporary table of
     `nvarchar(max)` columns named by us, then one `INSERT … SELECT … ORDER BY` per batch. A bulk
     load straight into the table would need every value typed as its column on the client;
   - SQLite: one prepared single-row `INSERT` run for each row, in-process;
   - DuckDB: its appender, given the column names.

   The server converts the text to each column's type, so its own rules and errors apply. The
   core converts only what servers disagree on: `true`/`false` to `1`/`0` for boolean, `bit` and
   `tinyint(1)` columns, dates and timestamps written in the chosen formats to ISO, and the chosen
   time zone added to timestamps without one where the column keeps a zone. MySQL and MariaDB
   have no type that keeps a zone: their timestamps with one are stored in UTC. A SQLite file on
   a server (ADR-018) is reached through its `sqlite3` shell, which cannot bind values: it cannot
   be imported into (the capability `insert_rows` says so).
6. **What runs is shown first** (ADR-026): `DROP TABLE` (replace), `CREATE TABLE` for a new table,
   `ALTER TABLE … ADD` for new columns of an existing one, and the statement the rows go through
   (`COPY …` or `INSERT … VALUES (?, …)`), with a note that values travel apart. Running sends the
   same request again: the core writes the statements again, never taking SQL from the UI.
7. **Safeguards are the core's, like any write.** A read-only connection refuses an import in
   the core, before anything runs. The guard (`guard.rs`) reads every statement of the plan, the
   rows' statement as the `INSERT` it is: replacing a table is a `DROP` and always asks; where the
   connection's type confirms data changes (Production) the insert asks; where it confirms
   execution everything asks. The app shows the core's list in the usual confirmation and the
   core refuses an unconfirmed run.
8. **A new table whose name is taken** fails, appends (columns that exist are used as they are,
   new ones are added) or replaces (drop, then create; it asks first).
9. **Batches and transactions.** The import runs on a session of its own, opened from the
   explorer's (or the tab's) like an export's (ADR-034): never an editor tab's, which may hold a
   transaction (ADR-031), and its manual and smart commit do not apply. The DDL runs first, each
   statement as written. Then the rows go in transactions of `commit_every` rows (50,000 by
   default; 0 for one transaction), each committed when complete. The source is read in a
   blocking thread, 5,000 rows at a time, with at most two batches waiting, so memory does not
   grow with the file.
10. **Cancel and errors leave only whole batches.** Cancel stops the running statement (the
    engine's cancel) and the load, and rolls back the transaction it was in; a failed batch is
    rolled back too. The result says how many rows were committed before, and which rows failed
    (a batch's first and last row). A table the import created stays, with the committed rows.
    Nothing is retried or re-run.
11. **After an import** the explorer reads the schema again when a table was created, and the
    dialog offers to open the table.
12. **Entry points:** the explorer's table menu (Import data…: into that table), its schema menu
    (Import data into a new table…), and Ctrl+Alt+V (Import data from the clipboard, in the
    command palette too), which opens the dialog with the clipboard's text for the active tab's
    connection.

## Left out (each can be an item of its own)
- Truncating before loading, "on duplicate key" (ignore, replace, update), switching off foreign
  key checks, and column transforms.
- XLSX and other formats, JSON Lines, and remembering options between imports.
- Agents cannot import: the MCP keeps its own rules (ADR-019).

## Alternatives
- **Literals in multi-row `INSERT` text** (what `edit.rs` writes for a few typed cells): fast and
  simple, but every value of an untrusted file would become SQL text; one escaping mistake is an
  injection. Binding removes the class of bug.
- **`LOAD DATA LOCAL INFILE` on MySQL:** the fastest path, but off by default on both sides for a
  good reason; prepared multi-row inserts load a million rows in seconds.
- **Typed TDS bulk load on SQL Server:** faster still, but the client would have to parse every
  value as the column's type (and get dates, decimals and collations right) instead of the
  server; the staging table keeps the server's conversions with most of the speed.
- **One transaction for the whole import:** all or nothing, but a long transaction on a big file
  holds locks and log space; it stays available as `commit_every = 0`.
- **Importing on the explorer's session:** no new connection, but it would block the explorer for
  the length of the import.

## What would change this
- If users need the server's fastest paths (`LOAD DATA`, typed bulk loads), an opt-in per
  connection, with the trade-offs said.
- If the detection needs to see the whole file (a value far down that does not fit), a full scan
  before the load, at the cost of reading the file twice.
