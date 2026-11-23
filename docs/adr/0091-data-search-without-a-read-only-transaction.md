# ADR-091: The data search on a server without a read-only transaction: base tables only, a fixed statement, the value bound

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-23 (accepted 2026-11-27, chosen over the proposal in #475)
- **Extends:** ADR-043 (the data search reads each table inside the engine's read-only
  transaction) and ADR-020 (servers that do not enforce their engine's read-only modes get no
  reads that rely on them)
- **Builds on:** SDB-170 and SDB-344 (`server_family.rs` recognises the server a MySQL
  connection reached), SDB-354 (edits of several statements and imports are refused where
  transactions do not hold)
- **Decides:** SDB-355

## Context
The data search (ADR-043) reads each chosen table with one statement the core writes, the
value bound as a parameter, inside the engine's read-only transaction (ADR-034's barrier). On a
server that accepts that transaction without enforcing it, `read_only_transaction` refuses the
read before it is sent (ADR-020): "GreptimeDB does not enforce read-only transactions". So the
search cannot run on GreptimeDB, TiDB, SingleStore, Doris, StarRocks, Databend, CrateDB,
Materialize or an unchecked fork, whatever the plugin can bind.

The transaction is there because the classifier sees the statement's text, not what runs
behind it: a function the statement calls, or a view it names, may write. The data search
differs from a user's query in what the text can contain: the core writes all of it, from a
template, and only three things come from outside — the table's name, its columns' names
(both quoted by the dialect) and the searched value (bound, never in the text). What runs
behind the statement is then only the template's own functions and the read of the named
object. Whether that can write is a question about one engine, answered by checking it.

### GreptimeDB 1.3.1, checked
- **Functions that change the server run inside a plain `SELECT`.** `SELECT flush_table('t')`
  and `SELECT compact_table('t')` run and return 0, and so does a `SELECT * FROM t WHERE
  flush_table('t') = 0`. `EXPLAIN` plans these as ordinary functions: `flush_table`,
  `flush_region`, `compact_table`, `compact_region`, `migrate_region`, `reconcile_table`,
  `reconcile_database`, `reconcile_catalog`, `build_index`, `flush_flow`, `gc_regions`
  (`ADMIN …` is only another way to call them). None of them is in the data search's template.
- **A view may name them.** `CREATE VIEW v AS SELECT flush_table('t')` is accepted. Reading it
  fails in 1.3.1 ("Missing TableMutationHandler"), but nothing promises that stays so, and a
  view's text is the server's, not the core's: views are never read by this path.
- **No user-defined functions.** `CREATE FUNCTION` is "not supported"; the Python scripts of
  earlier releases are gone. `information_schema.routines` is empty.
- **What a table is.** `information_schema.tables` says `BASE TABLE` with engine `mito` (a
  table) or `metric` (a metric engine table, physical or logical), `VIEW` for a view, and
  `LOCAL TEMPORARY` for `information_schema`'s own tables. External tables (engine `file`) read
  files under the server's copy root; their `CREATE` was refused here for a path outside it, so
  they were not read, and they are left out.
- **Flows** run on their source table's writes or on their own interval, scheduled by the
  server; a read of the source or the sink does not run them.
- **`START TRANSACTION READ ONLY` is accepted and writes still go through** (SDB-344), which is
  why the search is refused today.

## Decision
1. **A server family may let the data search run without the read-only transaction only when
   it is checked, as above, that the search's own statements cannot write there.** Today that
   is GreptimeDB (1.3.1) alone, a property of `ServerFamily` in the core, false for every other
   family and for unchecked forks. Everywhere else the search stays refused as it is, with the
   same reason. Agents, streaming exports, column statistics and read-only connections are not
   changed: they read what the user or an agent typed, which this argument does not cover.
2. **Only base tables of the server's own storage.** Right before a table is searched, the
   core reads its type from the server's catalog (`information_schema.tables`, the schema and
   name bound): it must be `BASE TABLE` with engine `mito` or `metric`. A view, an external
   table, a system table, a table that has disappeared or anything else is reported as not
   searched, with the reason, and the search goes on. The explorer's kind is not enough: it is
   a cached list, and it calls anything that is not a view a table.
3. **The statements are exactly the template's.** The probe (`SELECT * FROM t WHERE 1 = 0`) and
   the search (`SELECT` of the key, `CASE WHEN … THEN 1 ELSE 0 END` flags and snippets, `FROM`
   the table, `WHERE` the matches, `LIMIT`) call only functions of a fixed list chosen for the
   engine — for GreptimeDB `CAST`, `lower`, `strpos`, `substr` and `GREATEST`, all pure — and
   nothing else; identifiers are quoted by the dialect, and the value is a placeholder. Each
   statement is still checked as one read (`checked`), and a test asserts that every statement
   the template can write names only functions of the list, so a change to the template that
   adds one fails before it ships.
4. **The value is bound by the server's own placeholders.** GreptimeDB has no user variables
   (the MySQL plugin binds to `@spelaion_p1`), so the plugin sends the statement prepared, with
   one `?` per occurrence of the placeholder, found by the dialect's scanner (ADR-065), never by
   a text search, so a name that contains `@spelaion_p1` is not touched. On this path the
   template returns every column as text, the key's too, so the prepared statement's results
   need no second decoder for the types (the reason ADR-043 gave against MySQL's binary
   protocol); the key's types come from the probe, which is not bound.
5. **Still bounded, cancellable and in a session of its own** (ADR-043's point 4), opened for
   reads only, without the transaction around each table.
6. **Imports and edits of several statements stay refused** where transactions do not hold
   (SDB-354): they write, and nothing above makes them atomic.

## Alternatives
- **Keep the search refused on every such server (the status quo):** safe, and the search is
  most useful on exactly the analytical servers that lack the transaction. A checked argument
  per engine costs one card per engine and keeps the default a refusal.
- **Trust the classifier alone:** it reads the text, and a view or a function hides what runs.
  The argument here holds only because the core writes the whole text and reads only base
  tables.
- **Allow views when the server reports their definition:** the definition would have to be
  classified each time, and GreptimeDB already lets a view name a function that changes the
  server. Views can be added by a later ADR if a server can be shown to keep them from writing.
- **Search through GreptimeDB's PostgreSQL port** (where `$1` binds): the user chose a MySQL
  connection, and a second connection to the same server is something to configure, not a
  fallback to open silently.
- **Write the value as a literal on servers that cannot bind:** rejected by ADR-043 for every
  engine; nothing here changes that.

## Consequences
- The data search works on GreptimeDB once the implementation card lands: GreptimeDB's variant
  of the search's SQL (its `CONVERT … USING` and `COLLATE` are not supported there), the catalog
  check of point 2, the `?` binding in the MySQL plugin, and tests that try to get around it in
  `tests/mysql_family.rs` (a view over `flush_table`, an `information_schema` table, a name
  containing the placeholder, a cancel).
- Another server joins only with its own check, written into this ADR as a dated note, as
  ADR-020's table does for the read-only modes.
- If a later GreptimeDB adds user-defined functions, writable external tables or anything a
  base table's read can trigger, the property is withdrawn for it until checked again; the
  mysql_family tests pin the release they ran against.
