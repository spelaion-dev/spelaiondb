# ADR-034: Exports that stream to a file: a new read-only run, written batch by batch

- **Status:** Accepted (implemented)
- **Date:** 2026-11-07
- **Builds on:** ADR-005 (Arrow batches), ADR-008 / ADR-020 (read barriers), ADR-010 (results
  in the core), ADR-014 (table data tabs), ADR-028 (sessions), ADR-029 (connection types),
  ADR-031 (transactions)

## Context
The export button wrote a result that was already in the result store, so a big result had to
be fetched to its last row first (and kept in memory, then on disk) before it could be
exported, and a table could not be exported without opening it. Users also want CSV the way
their spreadsheet reads it (`;` in much of Europe, another NULL text, Windows-1252), only the
rows or columns they selected, and protection from CSV ("formula") injection: a text cell
that starts with `=`, `+`, `-` or `@` runs as a formula when a spreadsheet opens the file.

## Decision
1. **Two paths.** A result that is already loaded is still exported from the store, as before
   (also a selection of it: its rows and its shown columns). Everything bigger goes through a
   **streaming export**: the query runs again and each Arrow batch is written to the file as it
   arrives and then dropped, before the next one is taken. The export holds one batch at a time
   (10,000 rows; the drivers' own queues hold at most four more), never the row count; the
   summary reports the most rows it held (`peak_rows`), which the tests check against the batch
   size.
2. **What can be streamed:** the query of a result (the statement that produced it), a table
   data tab (its `WHERE` and `ORDER BY`, every row, not just the loaded page), and tables and
   views chosen in the explorer (one file each, into a chosen folder). For tables the core
   writes the `SELECT` with `table_export_sql`: the table data tab's checked `SELECT` without
   its paging, never SQL from the UI.
3. **A new, explicit run.** Re-running is something the user picks in the export dialog ("All
   rows: run the query again"); nothing ever re-runs by itself (the same rule as ADR-028). The
   dialog says that the query runs again in a new session. Before offering it, the core checks
   the statement (`rerun_check`): the result of a write (`INSERT … RETURNING`, a procedure call)
   can only be exported from the rows already loaded, and the dialog says why. If the query
   fails in the new session (a temporary table of the tab, for example), the dialog shows the
   error and offers to export the loaded rows instead.
4. **Its own session.** The export opens a new session from the tab's (the same resolved
   connection, tunnel and in-memory password) and closes it at the end. It finds unqualified
   names where the tab does (`follow_session_context`): the tab's database on MySQL, MariaDB and
   SQL Server, its schema on PostgreSQL and DuckDB; the tab is only asked which one it uses, a
   catalog read. Otherwise the tab's session, which may hold an open transaction (ADR-031), is
   never used or disturbed. So the export sees committed data only,
   and none of the tab's session state (`SET`, temporary tables, an uncommitted change). When
   the tab has an open transaction, `rerun_check` says so and the dialog warns that its
   uncommitted changes will not be in the file.
5. **Only reads are streamed.** Before anything is sent, the statement must be exactly one
   statement for the engine's splitter and a read for `classify_read_only` (the agents'
   classifier: nothing nested that writes, no `SELECT … INTO`, no `FOR UPDATE`, nothing that
   reaches outside the database, no MySQL executable comments). Then it runs inside the
   engine's own read barrier, always rolled back: `BEGIN READ ONLY` on PostgreSQL, `START
   TRANSACTION READ ONLY` on MySQL and MariaDB, a transaction that is always rolled back on SQL
   Server, `PRAGMA query_only` on the export's own SQLite connection, and `BEGIN TRANSACTION
   READ ONLY` on DuckDB. DuckDB's `enable_external_access = false` is not used: clones of a
   DuckDB file share one database (ADR-016), so it would switch it off for the user's tabs too;
   the classifier already refuses DuckDB's file and network functions by name. There is no row
   limit and no time limit: the user cancels.
6. **Connection types (ADR-029).** A type that confirms execution asks before a query is run
   again, and the core refuses an unconfirmed one (`check_script`, as for any run). A read is not
   a data change, so Production asks nothing more. Tables and table data tabs are exported with
   the core's own `SELECT`, which asks nothing, exactly as opening a table data tab.
7. **Progress and cancel.** The export reports rows and bytes written (at most ten times a
   second). Cancel stops the driver's statement (the engine's cancel) and the writer. The file is
   written as `<name>.part` and renamed only when complete: **a cancelled or failed export
   leaves nothing**, the `.part` file is deleted and a file that was already there is
   untouched. A partial file is never left under the final name.
8. **Folders of tables.** One file per table, named after it (characters that file systems
   refuse become `_`; Windows' reserved names get a `_` too). An existing file is never
   overwritten: the new one is called `orders (2).csv`. Tables are exported one after the other
   on the same export session; cancel stops the one running and skips the rest, keeping the
   files already finished.
9. **CSV and TSV options**, in the core: delimiter (CSV only; any character but a quote or a line
   break), header row, NULL text (empty by default), encoding (UTF-8 with or without byte order
   mark, UTF-16 LE with its mark, or Windows-1252, where characters it lacks become `?` and are
   counted in the summary), and formula escaping. JSON, SQL and Markdown are always UTF-8.
10. **Formula escaping is on by default** for CSV and TSV files (an opt-out). A text cell, or a
    column name, that starts with `=`, `+`, `-`, `@`, a tab or a carriage return gets a leading
    `'` (OWASP's advice), so a spreadsheet shows it as text instead of running it. Numbers are
    never touched: number columns are skipped, and so is text that is just a number (`-5`,
    `+1.5e3`). The cost is that such a cell no longer round-trips exactly, which is why it can be
    switched off; the harm it prevents (a formula that runs a program or sends data away on
    opening) is worse and the people who open exports in spreadsheets rarely know about it.
    Copies to the clipboard are not escaped: they are pasted by the user, who sees them.

## Left out
- Re-running a query that is not a read (a procedure, `INSERT … RETURNING`): export what is
  loaded instead.
- Remembering the CSV options between exports, and other formats (Excel, Parquet): future work.
- The query history does not record the export's run: it is the same query the user already ran.

## Alternatives
- **Streaming on the tab's own session.** It would see the tab's temporary tables and its
  uncommitted changes, but it would block the tab for the length of the export and could mix
  with a transaction the user left open (ADR-031).
- **Spilling the whole result to disk and exporting from there.** The result store already does
  this, but it still fetches and converts every row into the store first and keeps a second copy
  on disk; streaming writes the file directly.
- **Leaving a cancelled export's partial file, marked.** Nobody wants half a table, and a file
  that looks complete is a trap; deleting it is simpler and safer.
- **Formula escaping off by default.** DBeaver does not escape; we chose safety for the people
  who open the file, and keep exact data one checkbox away.

## What would change this
- If users need the tab's temporary tables or uncommitted changes in an export, an explicit
  option to stream on the tab's session (blocking it) could be added.
- If the read barrier of an engine changes for agents (`readonly.rs`), this one follows it.
