# ADR-044: Pending edits live in the core, with bulk edits of a selection and undo / redo

- **Status:** Accepted (implemented)
- **Date:** 2026-11-10
- **Supersedes:** the "pending changes live in the UI" decision of ADR-014 (the rest of ADR-014
  stands: primary key from the database, one transaction, one row per statement)
- **Builds on:** ADR-010 (results live in the core), ADR-029 (Production confirms data changes),
  ADR-031 (edits are a transaction of their own); fits ADR-041 (nothing here crosses the driver
  contract)

## Context
ADR-014 kept a table data tab's pending changes in React: "a few typed values, deleted and added
rows", the user's input and not result data, so the core gained nothing from holding them. Bulk
edits change that. Filling a column down, trimming or changing the case of a thousand cells,
setting them to NULL, DEFAULT, a new UUID or now, and pasting a block copied from a spreadsheet
all compute new values from the values on the page, which live in the core (ADR-010). Doing that
in React would mean pulling the page's values into React state, the thing ADR-010 forbids, and
writing type checks there that the core already has. Undo and redo of those edits must restore
exactly what was pending, on a page that could also be re-sorted or filtered under them.

## Decision
1. **Pending edits live in the core, with the stored result of the page they belong to**
   (`StoredResult::pending`, `pending.rs`): cell values, rows marked for deletion and rows to
   insert, by position in the page. Freeing the result (a new page, a refresh after applying)
   drops them; applying them clears them and their history.
2. **The UI sends what the user does and shows what the core holds.** Each action (a cell typed,
   rows deleted or reverted, a new row, a bulk edit, a paste, Discard, undo, redo) is one
   `PendingEdit` sent to the core, which answers with the edits now pending. React keeps that
   answer only to draw the grid and the cell panel, never the page's values.
3. **Bulk edits act on the selection's block** (its shown columns, never hidden ones; rows
   marked for deletion are left out): set a value (NULL and DEFAULT included), fill down (the
   first row of the block, or the row above a one-row block), trim, upper and lower case (text
   columns only), a new UUID per cell, and now (one instant, written for each column's type; the
   time zone of a `timestamptz` as the grid shows it, UTC). A paste reads tab-separated text as
   spreadsheets copy it: one value fills the selection, a larger block goes from the selection's
   first cell down the page and to the right, and what falls past the page is left out and
   counted. An empty pasted field is NULL except in a text column, where it is empty text.
4. **Bulk and pasted values are checked when they are given**, by the same conversion that
   writes them, so a bulk edit or a paste with a value its column cannot take changes nothing
   and says why. A value typed into one cell is checked when it is applied, as before, so typing
   is never refused half way (`-` before `-5`). A value equal to the database's takes the cell
   back instead of staying pending.
5. **Every action is one undo step** (at most 500 kept), stored as the changes it made with
   their values before and after, so undo and redo restore the pending edits exactly; typing on
   in the same cell joins its step. Discard is a step too, so it can be undone.
6. **The view cannot change under pending edits**: sorting or filtering a result with pending
   edits is refused in the core, since a position would then name another row.
7. **DEFAULT** is written as `SET column = DEFAULT` (a new row leaves the column out). SQLite
   has no `DEFAULT` there, so the column's default expression is written as its catalog holds it,
   or NULL without one, which is what SQLite would store.
8. Applying is unchanged from ADR-014: the core writes one statement per row by primary key from
   the edits it holds, runs them in one transaction where each must affect exactly one row,
   asks for the Production confirmation (ADR-029) and refuses on read-only connections.

## Alternatives
- **Keep pending edits in React** and compute bulk values there: needs the page's values in
  React state (against ADR-010) and type checks written twice.
- **Snapshots for undo** (a copy of everything pending per step): simplest, but a few bulk edits
  of a large page multiply memory; the changes of each step are enough to go both ways.
- **Bulk edits as SQL** (`UPDATE … SET c = upper(c) WHERE pk IN (…)`): fewer statements, but
  they break ADR-014's one row per statement check, differ per engine, and could not be shown or
  undone cell by cell before they run.
- **Key pending edits by original row** instead of position: survives a re-sort, but every
  window, statement and selection is by position; refusing to re-sort while edits are pending is
  simpler and says what to do.

## Revisit if
Pending edits must survive a refresh of the page (they would have to be keyed by primary key
value), or pages grow beyond what one transaction of single-row statements applies in
reasonable time.
