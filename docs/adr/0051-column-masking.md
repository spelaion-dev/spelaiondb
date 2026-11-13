# ADR-051: Column masking: values replaced where results enter the core, revealed one cell at a time

- **Status:** Accepted (amended by ADR-052: rules also match the table column a result column
  reads, and `table.column` rules are allowed)
- **Date:** 2026-11-11
- **Builds on:** ADR-010 (results live in the core), ADR-020 (agents' reads), ADR-022 (trust
  pinned to content), ADR-029 (connection types, safeguards only add up), ADR-033 (agents'
  output), ADR-034 (streaming exports), ADR-044 (pending edits in the core)

## Context
Teams that hold personal or payment data want to share a screen, record a demo, send an export
or let an agent query a database without the values of some columns showing: passwords,
e-mails, card numbers, national ids. Every database client that offers masking does it where
the value is drawn, and that is the problem: a value the grid hides is still sorted, filtered,
searched with Ctrl+F, summed by the calc panel, copied, exported, charted and handed to agents
by paths that each read the stored result on their own. If one of them forgets, masking is
theatre.

SpelaionDB already keeps every result in the core and serves the UI windows of it (ADR-010),
and the paths that do not read the result store (streaming exports, column statistics, the data
search, agents' reads) are a short, known list in the core. That makes a stronger design
possible.

### What masking is, and what it is not
Masking keeps the values of chosen columns **out of the app's output as a side effect of normal
work**: browsing a table, running a query that selects those columns, copying, exporting,
looking at statistics, sharing the screen, letting an agent query the database.

It is **not access control.** Whoever can run SQL on the connection can get any value past it on
purpose: an alias or an expression (`SELECT email AS e`, `lower(email)`), a backup (ADR-049), a
function on the server. The barrier for that is the database's own: column privileges, views,
or the engine's masking (PostgreSQL Anonymizer, SQL Server Dynamic Data Masking), just as a
read-only database user stays the real guarantee for agents (ADR-020). The app says so where
masking is set up.

## Decision
### Which columns are masked
1. **Rules match result column names**: case-insensitive glob patterns over the whole name,
   with `*` (any characters) and `?` (one character); a pattern without them is an exact name.
   `*password*` masks `password`, `user_password` and `PasswordHash`. Columns are matched by the
   name the result gives them, the only name every engine reports for every result; a table
   data tab's columns are the table's.
2. **Three places hold rules:**
   - **The connection's `[masking]` table**: `always` (patterns to mask) and `never` (patterns
     to leave visible, for names a broad pattern catches by mistake, such as `email_verified`).
   - **The user's `user.toml`**, `[masking] always`: patterns for every connection on this
     computer (`*password*` everywhere).
   - **A connection type** (ADR-029), `mask_columns`: patterns every connection of that type
     masks.
3. **How they combine:** a column is masked when a *required* pattern matches it, or when an
   *exemptible* pattern matches it and no `never` pattern does.
   - Exemptible: the connection's own `always`, and the user's patterns when the connection is
     the user's own (personal).
   - Required: the connection type's `mask_columns`, and the user's patterns on a repository
     connection: a file from the repository cannot unmask what the user or the type masks.
4. **A connection type can require masking, and it only adds up**, like its other safeguards:
   `mask_columns` from every layer that defines the type (built-in, team, personal) are all
   applied, and no connection's `never` exempts from them. The built-in types require nothing:
   nothing changes until someone writes a rule.
5. **Rules from a repository follow trust (ADR-022), and nothing missing unmasks.** A repository connection's `[masking]` is
   part of the connection's snapshot like every other field, so a `git pull` that adds a `never`
   is reviewed before the connection is used again. A team type's `mask_columns` is a field of
   the `connection_types` snapshot (written only when not empty, so snapshots trusted before
   this ADR stay unchanged). Rules are checked when files load: at most 100 patterns per list,
   128 characters each, no empty pattern and no hidden or control characters.
   - A `user.toml` that does not load masks **every** column of every connection until it is
     fixed: its patterns are unknown, and a missing rule must not unmask anything (like ADR-029's
     broken types file).
   - A connection pasted from the clipboard (ADR-030) keeps its `always` but drops its `never`:
     it becomes the user's own, whose `never` would unmask the user's patterns.

### Where it is applied: where results enter the core
6. **Every result a session stores is masked as its batches arrive**, before the result store
   keeps them. The masked columns are replaced in the stored batches by the mask, and the column
   becomes text with the metadata `spelaion.masked = "true"` (its native type is kept for the
   header). The original values are kept aside in that result, readable only by revealing one
   cell (below). So the grid's windows, sorting, the text filter and column filters, Ctrl+F, the
   calc panel, charts, maps, the cell viewer, copies (text, HTML, `IN` lists) and exports from
   the store all work on the mask: there is no value in what they read, so a path added later
   cannot leak it by forgetting a check.
   - It covers the editor's runs and the core-written statements that show rows (table data
     pages, "load more" pages, the references panel). Internal reads of fixed catalog or session
     metadata are not masked; core reads that parse numbers (statistics, counts) read their own
     statements unmasked and apply the rules themselves (part 7).
7. **The paths that do not go through the result store apply the same rules themselves**, each
   with its tests:
   - **Streaming exports** (ADR-034) mask every batch before it is written.
   - **Column statistics** of a masked column report rows, NULLs and the distinct count; the
     minimum, maximum and most frequent values are the mask, and the most frequent values carry
     no "filter by this value" condition (it would hold the value).
   - **The data search** (ADR-043) still finds rows by a masked column's value (the user typed
     it), but shows the mask in place of the column's snippet, of a masked key value, and writes
     no filter from a masked value.
   - **The foreign key value picker** shows the referenced table's columns masked like any
     page, and is refused when the key itself has a masked column.
   - **Agents' reads** (`run_query`, ADR-020) mask the rows `run_read_only` returns, and the
     JSON and TOON output marks the column as masked, so the agent knows the value is hidden
     rather than a string of dots.
8. **The mask is `••••••`**, six characters whatever the value, so its length tells nothing.
   **NULL stays NULL**: whether a value exists is not the value, and hiding it would make the
   grid lie about NULLs. Execution plans are not masked: they show the statement and the plan,
   not rows.
9. **What would compute from the mask, or leak through order, is refused** in the core, with a
   message that names the masked column:
   - editing a masked cell, and bulk edits, pastes or a file load that touch one (ADR-044):
     writing the mask back, or a value derived from it, would destroy data;
   - editing or deleting rows of a table data tab whose primary key has a masked column, and
     following a foreign key through a masked column: the statement or the filter would need the
     value;
   - "filter by this value" on a masked cell, and picking a foreign key value through a
     masked key column;
   - sorting a stored result by a masked column: it would order nothing. A table data tab
     sorts on the server, where the order would tell the values apart, so its header does not
     sort a masked column (the column's masked flag comes from the core).
   New rows can still be added; their masked columns are typed by the user, and a duplicated
   row cannot carry the mask as a value. The `WHERE` and `ORDER BY` a user types in a table
   data tab are their own SQL, like a query in the editor (see "What masking is").

### Revealing a cell
10. **Revealing is one cell, explicit and local.** The grid's context menu (and the cell
    viewer) offers "Reveal value" on a masked cell; the core returns that cell's original value
    from the result it keeps, without running anything, and the app shows it in a popover. It is
    never drawn in the grid and never remembered: closing the popover hides it again, and a new
    page or run is masked again. Copying from the popover is the user's own explicit act.
11. **Agents can never reveal:** the MCP server has no such tool, and agents' reads never keep
    originals.
12. **Revealing is not recorded in the audit log.** The log is the record of what agents did
    (ADR-033). Revealing is the user's own act on their own computer, on a connection where the
    same user can read the value with SQL; a record of it would suggest a control that masking
    does not give. If masking ever becomes a control enforced for someone else (rules the user
    cannot change, on a machine they do not own), recording reveals comes with it.

## Left out
- **`table.column` rules:** a query result does not say which table a column came from in every
  engine (PostgreSQL and MySQL do; SQL Server, SQLite and DuckDB do not, or not by default), and
  a rule that held only in table data tabs would leak in the editor.
- **Partial masks** (the last four digits of a card): they show part of the value and differ per
  kind of data; possible later as a per-pattern option.
- **Masking aliases and expressions** by tracing a result column to its source column.

## Alternatives
- **Masking where values are drawn or written out** (the grid, copies, exports each check a
  flag): what other clients do. Every path must remember the check, and sorting, filtering,
  Ctrl+F and the calc panel would still work on the real values, so a filter would tell what the
  hidden value is. Replacing the values where they enter the core removes them from every path
  at once.
- **Relying only on the database's masking:** it is the real barrier and we recommend it, but it
  needs a database administrator, differs per engine and does not exist in SQLite or DuckDB.
  Masking in the app is what a user can switch on for a screen share.
- **Recording reveals in the audit log:** see part 12.
- **Keeping the personal patterns in the local store**, next to ADR-033's switch: `user.toml` is
  already the personal configuration the resolver reads, and the MCP server reads it the same
  way.

## Revisit if
- Drivers report each column's source column everywhere (PostgreSQL and MySQL already can): an
  alias of a masked column could then stay masked.
- Teams need masking enforced against the user themselves: then reveals are recorded, a type may
  forbid them, and the rules must come from somewhere the user cannot edit.
