# ADR-079: An engine with no dialect of ours is read as standard SQL under its own name: no opaque submissions, no dialect picked by the user

- **Status:** Accepted (the reading is implemented, SDB-175; point 6's question is not)
- **Date:** 2026-11-24
- **Builds on:** ADR-020 (each engine's own read barrier), ADR-041 (dialects and barriers stay
  in the core; a plugin names one of the core's dialects, or none), ADR-056 (what the guard
  does with text the core cannot fully read), ADR-065 (one scanner per dialect)
- **Decides:** SDB-340

## Context
A generic Arrow Flight SQL connection may be any server. SDB-175's investigation (pull request
#261, closed unmerged) found that the core could not open an engine with no dialect:
`open_connection` asks `dialect_of`, and the session, the guard, parameters and the SQL the
core writes all read a `Dialect`. It named two ways out, an explicitly selected core dialect or
a core-owned *opaque submission* (the selection or the whole text sent as one confirmed
submission, never split or rewritten), and it asked for neither a fake dialect nor a barrier
inferred from the server's metadata.

SDB-175 then merged (#264, and #281 for InfluxDB 3) with a third answer that no ADR records.
What `dev` does today, checked:

- **`flightsql`, `dremio` and `gizmosql` each have `StandardDialect::named(<its own name>)`**
  (`spelaion-dialect`'s `standard.rs`): standard SQL's lexical rules, which are Trino's
  splitter. `'…'` strings and `"…"` names with doubled quotes, `--` and block comments, `;`.
- **No barrier answers to those names.** `Engine::has_read_barrier` is false for the three, so
  `read_only = true` is refused when the file loads (`load.rs`), a session cannot be made
  read-only (`session_read_only.rs`) and agents never see the connection
  (`AgentView::EngineWithoutBarrier`).
- **The guard reads each statement with sqlparser's generic dialect** (`sql_dialect`'s last
  arm) and falls back to the statement's first word. Where the connection's type confirms data
  changes, every statement `classify_read_only` does not accept as a read asks
  (`statement_risk`).
- **The plugin** (spelaiondb-plugin-flightsql 0.1.1) sends each statement once as a
  `CommandStatementQuery`, never as an update request and never a second time. It declares no
  transactions, no editing, no row loading, no bound values and no plans, and refuses results
  that an endpoint places on another server.
- **`:name` parameters are written into the text as literals by the core** (`bind_params`),
  with standard quoting, since the plugin binds nothing.
- `tests/flightsql.rs` and `tests/influxdb.rs` run a two-statement script split by the core
  against GizmoSQL, as `gizmosql` and as `flightsql`, and against InfluxDB 3.

So the question is no longer how to open such a connection. It is whether what merged is the
decision, and what it must not be mistaken for. Other people's plugins that name no dialect
are in the same place (ADR-041, point 5; ADR-065, point 5: they keep the standard reading).

## Decision
1. **An engine the core has no dialect for is read with the standard reading, under its own
   name.** "No dialect of ours" never meant "no reading": something has to say where a
   statement ends and what is a string. This ratifies SDB-175 as merged and is the rule for
   every later engine of that kind. The generic connection does **not** get an opaque
   submission mode, and the user does **not** pick a dialect for it.
2. **The reading is lexical, and it is a claim about nothing else.** It says where statements
   end, what is code and how a name is quoted. It is never another engine's dialect and never
   answers to another engine's name, because the core chooses its barriers by dialect name.
   - **No barrier, whatever the server says.** No read-only mode and no agents (ADR-020,
     ADR-041). `GetSqlInfo`, a server's read-only flag and Flight SQL's transaction actions
     never change that. A server gets a barrier only as a named engine with its own
     classifier and its own tests that try to get around it.
   - A named engine served by the same plugin (Dremio, GizmoSQL) keeps the standard reading
     until it has had its own check. GizmoSQL's name does not select DuckDB's dialect: its
     backend may be DuckDB or SQLite.
3. **What a run is.** The selection, or the whole text, is split by the standard reading.
   The statement under the cursor is found as on any engine. Each statement is sent as the
   exact text the guard read, once.
   - **History** records each statement as it ran, as on any engine.
   - **Session setup** (ADR-037) is allowed: it is the user's own text, pinned by repository
     trust, split the same way.
   - **`${name}` variables** work as on any engine: text the user wrote, checked against the
     same reading.
   - **`:name` parameters** stay, written as standard literals. Point 6 covers the one value
     that a server may read differently.
4. **The guard asks what it asks everywhere, from what it can read.** `DROP`, `TRUNCATE` and
   a `DELETE` or `UPDATE` without `WHERE` always ask. Where the connection's type confirms
   data changes, whatever the core cannot show to be a read asks: this is ADR-056's rule for
   what it cannot tell, and it is how `statement_risk` already behaves for these names. Where
   the type confirms execution, every run asks. The guard here is a help against slips, as
   everywhere, and it knows less than on an engine with a dialect: the form and the guide
   say so.
5. **The core writes little SQL for these engines, and never a data change.** A table opens
   with the core's `SELECT … LIMIT … OFFSET` and its filters; the explorer's actions show
   their statement before it runs (ADR-026). Editing rows, importing, bound searches, plans
   and transaction controls are off because the plugin declares none of them, and they stay
   off for an engine read this way: each needs values written or bound in a way the server
   is known to read. A plugin of ours for an engine with no dialect must not declare them.
6. **Where servers are known to leave the standard, the core asks instead of guessing.** A
   statement of an engine read this way that holds one of these markers asks before the
   script runs, on every connection type. The question shows the statements as the core split
   them and says the server may read them differently.
   - **A backslash directly before a quote** (`\'`, `\"`). MySQL's family, ClickHouse,
     Hive, Spark, BigQuery and others read it as an escaped quote; the standard does not.
     Without the question, `SELECT 'a\'b'; DROP TABLE t; SELECT 'c\'d'` is one statement
     to the core, whose first word is `select`, and three to such a server.
   - **A dollar quote** (`$$` or `$tag$`), which PostgreSQL's family and DuckDB read as a
     string or a body and the standard reads as code: a `;` inside it would cut it.
   - The markers are looked for in the raw text, also inside what the reading takes for a
     string or a comment, as the classifier's pre-checks do on purpose (ADR-065, point 3).
     A parameter value that ends in a backslash therefore asks too, since the guard reads
     the text after the values are written in.
   - Known and not marked, because ordinary text does not hold them: a `;` or a quote inside
     a backticked or bracketed name, `#` comments, nested block comments. The guide lists
     them.
7. **The query request, always, and once.** Every statement goes as a `CommandStatementQuery`.
   The core does not classify unknown SQL to choose between the query and the update request,
   and neither the core nor the plugin sends a statement a second time as the other, or after
   a lost connection: an error does not prove the first request changed nothing (ADR-028,
   AGENTS.md). A server that takes changes only as update requests refuses them, and its error
   is shown as it is.

## Options not taken
- **An explicitly selected core dialect** (the investigation's first option, and the most
  reasonable alternative). Splitting and the guard would be right for a server whose family
  the user knows. Against it:
  - the user must know what a lexical family is, and a wrong choice fails silently, in the
    same ways the standard reading does;
  - `sql_dialect`, `classify.rs` and `readonly.rs` choose their rules by dialect name. A
    picked "duckdb" must never become the name a barrier answers to, so the name and the
    family would have to be separated in the files that hold the barriers;
  - connection files written since 0.10 make no choice;
  - a server whose SQL we know well enough to choose for is better served as a named engine
    with its own check, which is the path SDB-175 already gives.
- **Opaque submissions** (the second option). Nothing is read, so nothing is read wrongly.
  Against it:
  - most Flight SQL servers take one statement per request, so a script would have to be run
    one selection at a time: no script runs (ADR-035), no statement under the cursor;
  - the guard could no longer ask about a `DROP`: it would ask about every run, which is a
    question people stop reading, or about none;
  - it replaces behaviour that is tested against three servers with a mode that has no user
    asking for it. Its confirmation is kept where it earns its place: point 6.
- **Leave it as merged and write nothing.** The next engine without a dialect would ask the
  same question, and the standard reading would sooner or later be taken for knowledge of the
  server.
- **Refuse a marked statement instead of asking** (point 6). `'C:\data\'` is a valid
  standard string, and the three servers we test read it as the core does.

## What would make us revisit
- **A server in real use whose scripts the standard reading splits wrongly, where point 6's
  question is not enough:** an explicit action, "run the selection as one statement", which
  sends it unsplit and always asks first. That is the opaque submission, for one run and by
  the user's choice, never as a connection's mode.
- **Several servers of one family reached through the generic connection** (MySQL's rules
  through Flight SQL, for instance): the first option above, as a choice on the connection
  that changes the reading and never the name.
- **A server that accepts changes only through the update request:** an explicit "run as an
  update" chosen by the user for that run. Never a classification by the core, never a retry.
- **A marker that asks too often on a server people use** (dollar quotes on a DuckDB
  backend): that server becomes a named engine with its dialect, and the marker stops
  applying to it.

## What follows as work items
None exists yet; each is created when this is accepted.

1. **The guard's question for a marker of another reading** (S, `guard.rs` and the standard
   dialect, so it waits for an owner): point 6, with property tests in which a statement
   hidden behind each marker never runs unasked on these engines, and no engine with a
   dialect of its own is asked.
2. **Say how the SQL is read** (S): the connection form of an engine read this way and the
   guide state what is read, what is not offered, and the known differences of point 6.
3. **A test in the plugin's repository that nothing is sent twice** (S): a connection lost
   after a statement was accepted reports that it may have run; an update the server refuses
   on the query request is reported as the server said it.
4. **Each named engine's own check** (M each, when wanted): Dremio, and GizmoSQL per
   backend, as SDB-175 asked. Each is its own decision; none is implied by this one.
