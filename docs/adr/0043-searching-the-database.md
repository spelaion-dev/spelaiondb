# ADR-043: Searching the database: names from the explorer's catalog, values bound in a read-only session

- **Status:** Accepted (implemented)
- **Date:** 2026-11-09
- **Builds on:** ADR-005 (the driver contract), ADR-024 (the explorer's catalog), ADR-034 (a read
  in its own session, inside the engine's read-only transaction); fits ADR-041 (engines as
  plugins): both additions to the contract take and return owned data and travel as messages

## Context
"Which table has a column called `customer_id`?" and "where does this value appear?" are common
questions with no good answer in the app. The first is about names, and the explorer already
reads most of them: the command palette lists what it has read (SDB-18) without asking the
database again. The second means reading every column of many tables: many statements, on
connections that may be read-only or Production, typed by someone who only wants to look.

The driver contract had no way to run a read with a bound value: catalog queries bind
parameters inside each driver, and the user's own SQL is text. Writing the searched value into
the SQL as a literal would make the core's quoting the only thing between a pasted value and
the server.

## Decision
1. **Metadata search answers from the explorer's catalog** (`catalog.ts`): tables, views,
   routines and other schema objects of the schemas the explorer has read, on every connection
   at once, matched by name in the UI like the command palette, never asking a database per
   keystroke. Columns are not in that catalog, so a new driver listing,
   `Connection::list_columns(schema)`, reads every column of a schema with one catalog query
   (a default that describes each relation serves engines without one). The search reads each
   loaded schema's columns once, when it first needs them, and keeps them with the schema: they
   go when the explorer forgets it (collapse, disconnect, refresh).
2. **A read with bound values is part of the driver contract**: `Connection::execute_bound(sql,
   params)` runs one statement with text values bound to the engine's placeholders
   (`bound_placeholder`: `$1` on PostgreSQL and DuckDB, `@P1` on SQL Server, `?1` on SQLite),
   and returns the same events and the same decoded values as `execute`. MySQL and MariaDB bind
   with a prepared `SET @spelaion_p1 = ?` and run the read with the text protocol, so values
   decode exactly as every other result's (their binary protocol would decode differently);
   the variables are cleared afterwards. `Capabilities::bound_params` says whether a connection
   can do it: SQLite over SSH (ADR-018) cannot, since nothing but SQL text may reach the
   server's `sqlite3`, so it has no data search. Like `execute`, `execute_bound` takes owned
   text and answers with the same events, so a plugin (ADR-041) carries it as one more message.
3. **Data search is written and run by the core** (`spelaion-core::data_search`). The user
   types a value, chooses a connection's tables (at most 1,000) and options; for each table the
   core reads its primary key from the catalog and its columns' types from a `SELECT * … WHERE
   1 = 0`, then runs **one** `SELECT` with the value bound as a parameter: a substring match in
   text and UUID columns (case sensitive or not, with the engine's position function, so no
   wildcard in the value means anything), equality in numeric columns when asked and when the
   value is a plain decimal, and, when asked, large text (JSON, XML, MySQL's `TEXT`, SQL
   Server's `text`, `ntext` and `xml`) and the bytes of binary columns. Dates, booleans,
   intervals, arrays and engine-specific types are not searched. Every statement is checked
   again as a single read.
4. **Always bounded, always cancellable, never a write.** Each table returns at most its row
   limit (100 by default, up to 1,000), and the whole search stops at 1,000 hits, which go to
   the UI with at most 120 characters of each matching column (as column statistics send their
   values). The search runs in a session of its own opened for reads only (ADR-037's
   `read_only_session`), each table inside the engine's read-only transaction (ADR-034's
   barrier), so it works on read-only connections and cannot change data. Cancel stops the
   running statement and every table after it; a table the engine refuses is reported and the
   search goes on.
5. **A hit opens the table data tab filtered to its row**: by the primary key
   (`table_data::key_filter`, as following a foreign key does) or, without one, by the value of
   a matching text or numeric column ("filter by this value"). The filter is checked like any
   other filter of that tab.

## Alternatives
- **Writing the value into the SQL as a literal** (as `bind_params` does for a template the user
  reads before running): one quoting mistake in one engine would let a pasted value become SQL.
  Binding is how the item asked for it and costs one method per driver.
- **MySQL's binary protocol for the bound read:** values arrive typed, and decoding them the
  way the text protocol does (dates, times, decimals, bits) would be a second decoder to keep
  in step with the type zoo.
- **One statement per column** instead of one per table: it says which column matched without
  `CASE` flags, but a 30-column table becomes 30 round trips through a tunnel.
- **`LIKE '%value%'`:** the value's `%`, `_` (and SQL Server's `[`) would need escaping per
  engine; position functions (`strpos`, `LOCATE`, `CHARINDEX`, `instr`) treat it as plain text.
- **Metadata search with catalog queries per keystroke:** exact, but over five connections and
  tunnels it is slow and loads servers for each letter typed.

## Revisit if
- Users want to search data in every table of a schema at once without choosing: then a bigger
  bound, with an estimate of the rows read first.
- Agents ask for it: then an MCP tool over the same core function.
- Snippets should show every match in a long value, or matches inside dates and other types.
