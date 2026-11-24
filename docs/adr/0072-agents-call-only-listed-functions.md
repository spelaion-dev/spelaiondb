# ADR-072: Agents call only the functions on a list: each engine's harmless built-ins, plus what the connection names

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Amends:** ADR-008 (layer 4, the classifier) and ADR-020 ("what reaches outside the database
  is refused by name"): for what an agent sends, a function must be on a list to be called. The
  list of refused names stays, above it, for agents and for everyone else
- **Builds on:** ADR-071 (the approval gate, which is not a way around this list), ADR-022
  (repository trust pins a connection's file), ADR-029 (safeguards only add up), ADR-065 (what
  the scanner is for, and what the SQL parser stays for)
- **Decides:** SDB-160

## Context
Every check in the agents' barrier refuses what it does not know, except one:

- statements: only what the classifier recognises as a read passes, and what does not parse is
  refused (`classify_read_only`, ADR-008);
- writes: only the kinds on a list (`write.rs`'s `allowed`, ADR-020, which says why: a deny list
  "lets through whatever nobody thought of");
- session setup: only the settings on a list (`setup_settings`);
- **functions: any function may be called unless its name is on a list of refused ones**
  (`reaches_outside` in `classify.rs`, with `outside_functions`, `outside_prefixes`,
  `sql_text_functions` and `sql_text_prefixes` per engine).

That last list has needed repairs each time someone looked:

- SDB-115 found three ways around it on PostgreSQL in one code review: functions that run SQL
  given as text (`query_to_xml`), names written with Unicode escapes, and names that were simply
  missing. Its fix added the names and says, in the module's own words, that the list "can
  never be complete".
- ClickHouse needed whole name prefixes, so that a table function a new release adds is refused
  too.
- RisingWave lists none of its built-in functions, so its list cannot be checked against the
  server. With a view able to hide a call as well, it got no agents at all (ADR-020, 2026-11-23).
- The servers that speak PostgreSQL's protocol and do get agents (CockroachDB, YugabyteDB) have
  functions of their own that the PostgreSQL list has never named.

Two facts about the code shape the decision:

- **`classify_read_only` is not only the agents' classifier.** Streaming exports, the data
  search, column statistics, execution plans, the guard and read-only connections call it for
  what the *person* runs. A person's own functions must keep working there.
- **A connection's file is pinned whole by repository trust** (`connection_snapshot` in
  `trust.rs` flattens every field), so a new field in `[mcp]` is reviewed like any other change
  to a team's connection (ADR-022).

SDB-116 is a different problem: there the SQL parser and a MySQL server disagreed on where a
string ends, so a call was hidden from any list. That is answered by making the session read
strings as the parser does (`mysql_default_quoting`) and by ADR-065, not by this decision.

## Decision
1. **What an agent sends may call a function only if the function is on a list.** It applies
   to `run_query`, `explain_query` and `execute_write`: the reads, the writes, and every
   statement nested in them (a view's query in a `CREATE VIEW`, a subquery, a CTE). A call to
   anything else is refused before anything is sent to the server.
2. **The list is the engine's built-in functions that only compute**, from their arguments and
   from the database's data inside the transaction. Left off, whatever their use: what reads or
   writes files, reaches other servers or the network, runs SQL given as text, changes the
   session or the server, takes locks that outlive the statement, or only waits.
3. **A connection adds its own names, in its file:** `[mcp] functions = ["st_*",
   "reporting.order_total", "analytics.*"]`.
   - An entry is a name, or a schema and a name. `*` may end a name (`st_*`) or stand for every
     function of one schema (`analytics.*`). A bare `*`, which would switch the rule off, is
     refused when the file loads.
   - An entry without a schema allows calls written without one. An entry with a schema allows
     calls written with that schema. The classifier cannot know which schema an unqualified
     call resolves to, so it never guesses.
   - It is a grant, like `exposed` and `writes`, and travels the same way: written by whoever
     owns the connection's file, pinned by repository trust, shown in the connection form and in
     the MCP page's Security tab. `spelaiondb check` warns when a connection whose type confirms
     data changes names functions for agents, as it does for `writes = "allow"`.
   - Nothing personal can add to a repository's connection, and no connection type can add
     names. That is how a team extends the list without loosening Production: the list is
     per connection, in a file the team reviews.
4. **The refused names stay, and win.** Today's lists keep refusing by name for everyone who
   uses the classifier, with their clearer message ("reaches outside the database"). A
   connection's entry can never allow a refused name: `functions = ["dblink*"]` allows nothing.
   The connection exposes one database to agents, not what that database can reach.
5. **It is the agents' rule only.** The person's own reads (exports, the data search,
   statistics, plans, a read-only connection's editor) keep today's check: they are the
   person's statements, and they call the person's functions. The connection's own session
   setup (ADR-037) is not an agent's text either and keeps its classifier.
6. **How a name is matched.**
   - By name, never by signature. Overloads share a fate: if one form of a name is harmful
     (`ts_rewrite` with two arguments runs a query), the name is off the list.
   - A name written with the engine's system schema (`pg_catalog.lower`) is the built-in. A
     name written with any other schema is allowed only by a connection's entry.
   - A quoted name is compared as written. `"LOWER"` is not `lower` on PostgreSQL.
   - A relation is checked only when it is called with arguments (a table function). Table
     names are not functions.
   - The parser's own nodes for built-in syntax (`CAST`, `EXTRACT`, `CASE`…) are not calls.
7. **What the agent is told.** The refusal names the function and says that it is not on the
   list of functions agents may call on this connection, that the query can be written without
   it, and that the user can allow it in the connection's settings. It has a code of its own, so
   the Activity tab can show which functions agents were refused. Nothing is retried.
8. **Engines.** Every engine agents reach gets a list, one engine per change: PostgreSQL (and
   the servers of its protocol, which use its list: their own functions are refused until a
   connection names them), MySQL and MariaDB, SQL Server, ClickHouse (scalar and table
   functions), Trino and Presto, and SQLite. **DuckDB keeps the refused names only:** its
   barrier is the engine's own setting (`enable_external_access = false`, ADR-020), which a
   function cannot get around, so a list would refuse queries without stopping anything.
   Until an engine's list lands, it keeps today's check.
9. **Where the lists come from, and how they are kept.**
   - Each is a data file in the core, generated by a script in the repository from the catalog
     of the server image the tests run against, where the engine lists its own functions
     (PostgreSQL's `pg_proc`, SQLite's `pragma function_list`, ClickHouse's `system.functions`
     and `system.table_functions`, Trino's `SHOW FUNCTIONS`). MySQL and SQL Server do not list
     their built-in functions in a catalog we know of: their lists start from the manuals'
     indexes.
   - A person reviews every name that enters: the files are named in `.github/CODEOWNERS` in
     the change that adds them.
   - On PostgreSQL the catalog also checks the review. A test against the real server fails
     if a listed function is not in `pg_catalog`, or is `VOLATILE` and not on a short reviewed
     set of harmless volatile ones (`random`, `gen_random_uuid`, `clock_timestamp`…).
   - A test per engine fails when a listed name does not exist on the tested server, so the
     lists do not rot. A server newer than a list has new built-ins that are refused until the
     list is regenerated. A connection's entry covers the gap meanwhile.
10. **No approval for an unlisted function.** The card's own recommendation was a list for
    what runs unattended and a person's approval (ADR-071) for the rest. ADR-071 decides that
    an approval never lets through what a barrier refuses and that reads are not gated. A
    person shown `st_distance` in a dialog knows no more about it than the person who writes
    the connection's file. The file is that judgement made once and reviewed by the team; the
    dialog is the same judgement clicked through forty times. What ADR-071 adds here is nothing
    new: a write that calls only listed functions still waits for approval where one is asked.

## What the list still cannot see
An object that already exists in the database can hide a call from any check that reads the
statement:

- a view, or a row security policy, whose definition calls a function (ADR-020's evidence for
  RisingWave and Babelfish);
- an operator or a cast that someone defined over a function;
- a function someone created with a built-in's name and other argument types, which the server
  may pick for an unqualified call;
- on PostgreSQL, a function called as if it were a column of its argument (`row.func`).

Each needs someone who may create objects in that database to have created one. An agent
cannot: `CREATE FUNCTION`, operators, casts and policies are not on the write list, and a view
an agent creates is checked by this rule. So the list changes the default from "allowed unless
known harmful" to "refused unless known harmless" for everything an agent can name directly.
It does not make the classifier a wall. The engine's read-only transaction stays under it, and
a read-only database user stays the only real guarantee (ADR-008, ADR-020).

## Options not taken
- **Keep the list of refused names, hardened (SDB-115).** It is the most reasonable
  alternative, and it costs nothing more: no lists to keep, and no useful query refused.
  - *Against it:* it is the one check that lets the unknown through. Every function an
    extension installs, every function a team wrote around `dblink` or an untrusted language,
    every built-in of a newer version and every function of a server that only speaks the
    protocol is callable until somebody thinks of it. SDB-115 was found by reading the code, not
    by a test.
- **Ask the server which functions are its own, when the session opens,** and refuse the rest
  unless listed. No lists to keep.
  - *Against it:* on a server that only speaks the protocol, its dangerous functions *are* its
    own. Several engines show a user only the routines that user has rights on. And a
    function's labels are claims by whoever created it: PostgreSQL does not check `STABLE` on a
    function written in C. The catalog is used to build and to test the lists, not to decide at
    run time.
- **Approval for what is not on the list.** See point 10.
- **A bare `*` for a connection**, to get today's behaviour back. It would be the wider door
  ADR-019 and ADR-020 rule out. `schema.*` is the widest entry.
- **Checking operators and casts too.** Built-in ones call built-in functions, and the ones
  extensions add (PostGIS's, pgvector's) would need every connection that uses them to list
  symbols. They join "what the list cannot see".
- **A list for DuckDB as well.** See point 8.

## What would change this
- **The lists refuse ordinary queries.** Before PostgreSQL's list is enforced, the statements
  of real audit logs (ours) are run through the rule and the refusals counted by function. If
  built-ins that only compute are refused, the list is wrong and is fixed. If most connections
  would need long lists of their own functions to stay useful, the rule is too strict as
  designed and this decision is reopened before it ships, not after.
- **The volatility check does not hold.** If a `STABLE` or `IMMUTABLE` built-in of PostgreSQL
  turns out to reach outside the database, beyond the ones that run SQL text, the PostgreSQL
  list is reviewed name by name with no help from the catalog, and its cost is judged again.
- **A function that hides behind a built-in's name is found in use.** Then an agent's session
  asks the catalog, when it opens, which listed names a schema outside the system's also
  defines, and refuses those unless the connection lists them.
- **Teams want a type to forbid extra functions** ("on Production, built-ins only"). That
  would be a safeguard of ADR-029, which only adds up.

## What follows (work items, created once this is accepted)
`classify.rs` is a CODEOWNERS file, and any change there needs tests that try to get around it.
1. The rule and its plumbing, with no list yet: the agents' check next to today's one, the
   matching of point 6, `[mcp] functions` in the connection file with its validation, the
   refusal's sentence and code, `spelaiondb check`'s warning, and `docs/guide/mcp.md`.
2. PostgreSQL's list: the generating script, the reviewed file, the catalog test of point 9,
   and the replay of real statements, whose numbers go in the pull request before it merges.
3. MySQL and MariaDB.
4. SQL Server.
5. ClickHouse, Trino and Presto.
6. SQLite, local and on a server (the `sqlite3` shell adds functions of its own).
7. The connection form's field and the Security tab's column, and the Activity tab's view of
   refused functions.
