# ADR-014: Editing table data

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
A database client is expected to change rows in place: edit a cell, delete a row, add one.
Writing is where a client can do damage, so the rules that keep the rest of the app safe
(security in the core, nothing re-run automatically, production asks first) must hold here
too. Results may come from any query (joins, expressions, `SHOW`), and only some of them
identify the rows of one table.

## Decision
- *(The restriction to table data tabs is superseded by [ADR-061](0061-editing-query-results.md).)*
  **Only table data tabs are editable**, and only for tables with a **primary key**. Their
  rows come from the SELECT that the core writes and checks (`table_data.rs`: one plain
  SELECT of that table), so every row maps to one row of the table. Query results and
  tables without a primary key stay read-only: guessing a row from all its columns can
  change several rows at once.
- **Pending changes live in the UI** until applied (superseded by ADR-044: they live in the
  core, with the stored page, so bulk edits and undo can use its values): they are the user's input (a few typed
  values, deleted and added rows), not result data. Edited cells and deleted rows are drawn
  differently; **Apply** sends them, **Discard** drops them, and paging waits until one of
  the two happens.
- **The core writes the statements** (`edit.rs`): it describes the table itself (the
  primary key is never taken from the UI), reads the key values of each row from the stored,
  typed result (not from the text on screen), converts typed values by column type (numbers
  checked and unquoted, booleans, `\x…` bytes, the rest as quoted text the server casts) and
  quotes identifiers for the engine. `UPDATE … WHERE pk = …`, `DELETE … WHERE pk = …` and
  `INSERT (only the columns given)`.
- **Applied in one transaction**: each UPDATE and DELETE must affect exactly one row and
  each INSERT one row; anything else (the row changed or vanished meanwhile, a trigger, an
  error) rolls everything back. MySQL reports matched rows (`CLIENT_FOUND_ROWS`) so an UPDATE
  that leaves a value as it was still counts its row.
- **Production asks first**: on a production connection the app shows the statements and
  the core refuses to apply them unless that confirmation was given, like `run_script`.
  Read-only connections refuse edits in the core.
- After applying, the page is loaded again; nothing else is re-run.
- The MCP keeps its own rules (ADR-008): this ADR does not give agents a way to write.
- **Where a transaction does not hold (2026-11-17, SDB-354).** GreptimeDB and CrateDB accept
  `BEGIN` and ignore it; an engine may have no transactions at all. There a batch that failed
  half way kept the changes before the failure while the app said it rolled back. Several
  changes at once are refused there, before anything runs (`Session::atomic_writes_refusal`,
  from `Capabilities::transactions` and `ServerFamily::enforces_transactions`); one change at a
  time still applies.

## Alternatives
- Editing any result whose columns map to one table (DBeaver): needs parsing the query and
  its aliases; more reach, more ways to edit the wrong row. Can come later on top of this.
- Keyless tables by all columns (Tabularis): a duplicate row changes twice; refused here.
- Pending changes in the core: no gain while they are only user input.

## Revisit if
Users need to edit results of joins or views with keys, or to edit keyless tables. The rules for
editing plain single-table query results are in [ADR-061](0061-editing-query-results.md).
