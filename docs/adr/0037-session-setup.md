# ADR-037: Every new session starts as its connection says: setup statements, default schema, isolation level

- **Status:** Accepted (implemented)
- **Date:** 2026-11-08
- **Amends:** ADR-028 (part 3: what reconnecting runs), AGENTS.md ("nothing is ever re-run")
- **Builds on:** ADR-008 (read-only barrier), ADR-022 (trust pinned to content), ADR-029
  (connection types), ADR-030 (pasted connections), ADR-031 (transactions), ADR-034 (exports)

## Context
Many teams need every session to start the same way: a role (`SET ROLE reporting`), a search
path, a time zone, a lock timeout, an isolation level. Today that has to be typed in each tab,
and typed again after every reconnect. DBeaver has "bootstrap queries" (with "ignore errors"),
a default catalog and schema, and a default isolation level per connection.

ADR-028 says reconnecting opens a new session "without running anything", and AGENTS.md says
nothing is ever re-run automatically after a reconnect. The point of that rule is the user's
statements: a statement re-run on a new session can run twice, or run without the ones before
it. Statements that make the session what its connection says are a different thing: they are
part of the connection, like its host or its user, and a session without them is not the
session the user asked for. That still needs an explicit decision, so here it is.

## Decision
1. **A `[session]` table in the connection file**, written by a new Session tab of the form:
   - `setup`: SQL run on every new session, split into statements by the engine's dialect, as
     the editor splits a script. At most 20000 characters, text without hidden characters. It
     is sent as written: `${…}` variables are neither expanded nor asked for in it.
   - `ignore_setup_errors`: a statement the database rejects is skipped instead of failing the
     session; the tab says which statements were skipped, with their error, until dismissed.
   - `schema`: the default schema, set as the explorer's "Set as the editor's schema" does:
     `search_path` (the schema, then `public`) on PostgreSQL, the database on MySQL and
     MariaDB, `SET schema` on DuckDB. SQL Server and SQLite have no schema per session, and the
     file refuses one there. `${user.…}` variables work, as in the other text fields.
   - `isolation`: `read-uncommitted`, `read-committed`, `repeatable-read`, `serializable` or
     `snapshot` (SQL Server only), set for the whole session (`SET SESSION CHARACTERISTICS …` on
     PostgreSQL, `SET SESSION TRANSACTION …` on MySQL and MariaDB, `SET TRANSACTION …` on SQL
     Server). SQLite and DuckDB have one level, and the file refuses one there.
   - Auto-commit is not here: it already comes from the connection type (`manual_commit`,
     ADR-031). A second, per-connection switch would contradict it.
2. **It is part of opening a session, every time.** `spelaion-core::open_connection` applies
   it right after the driver connects, in this order: isolation level, default schema, setup
   statements; the session is handed out only after that. That holds for every session the
   app opens for the connection (editor tabs, the explorer, table data tabs, completion's
   metadata session, streaming exports), for agents' sessions, and for a reconnect, which is a
   new session. It is the only thing a reconnect runs: **the user's statements are still never
   re-run** (AGENTS.md, ADR-028), and setup statements are run as the core's own: they are not
   in the history, never inside the manual-commit transaction (ADR-031), and their rows are
   not kept.
   - A default schema that does not end up being the session's schema (PostgreSQL accepts a
     `search_path` naming one that does not exist) fails the session, so a typo does not open
     tabs somewhere else in silence.
   - A setup that leaves a transaction open fails the session: no session starts inside one.
   - The whole setup is bounded by the connection's connect timeout; a lost connection during
     it fails the session, whatever "ignore errors" says.
   - When it fails, the session is closed (and its tunnel, if it was the last one) and the
     error says which statement and why. Explicit SQL wins: setup statements run after the
     default schema, and the tab's schema picker shows what the session really has.
3. **The read-only barrier applies to the setup** on a read-only connection (ADR-008), on a
   connection whose type confirms data changes (Production, ADR-029: safeguards only add up),
   and in sessions that only read (agents' read sessions, streaming exports, ADR-034). There,
   every setup statement must pass `classify_session_setup` (in `classify.rs`) **before any of
   them runs**: a read as `classify_read_only` decides, `USE`, or a session setting from a
   per-engine allow list (where names are found, how values are shown, timeouts, `SET ROLE`,
   an isolation level), with plain values only (literals and names, no expressions or
   subqueries). Anything else, a setting that could loosen read-only mode or change how SQL is
   parsed (`default_transaction_read_only`, `READ WRITE`, `sql_mode`, `SET NAMES`,
   `QUOTED_IDENTIFIER`…), `GLOBAL` scope, and whatever does not parse are refused. A refusal
   is never an error that "ignore errors" skips: the session does not open. The form runs the
   same check before saving (`check_file_setup`), so the user sees it there first.
4. **Trust and copies.** Every `[session]` key is part of what repository trust pins
   (ADR-022), like any field of the file: a pull that changes the setup, its "ignore errors",
   the schema or the isolation level is shown and must be accepted before the connection
   resolves again. A connection trusted before this ADR keeps its snapshot (an absent table
   adds no field). A pasted connection (ADR-030) never brings its setup statements along: the
   text is untrusted and they would run on the user's server at every open; the schema and
   isolation level only choose settings and are kept. Copying a connection to the clipboard
   leaves them out for the same reason. Duplicating keeps them (it is the same user's file).
5. **In the editor,** PostgreSQL and DuckDB tabs get a schema picker next to the connection
   picker (MySQL, MariaDB and SQL Server already have their database picker): it shows the
   session's schema, read after connecting and after each script, and switching it changes
   only that tab's session.

## Alternatives
- **Re-running nothing on reconnect, setup included.** Keeps the old wording, but a reconnected
  tab would silently be a different session (another role, another search path), which is
  worse than the problem ADR-028 solved: the user's next statement would run somewhere else.
- **Setup only in editor tabs.** Explorer, completion and export sessions would see other
  names than the editor; exports could even read other tables than the query the user ran.
- **A per-statement list (DBeaver's bootstrap queries).** A script split by the dialect is
  what people already write, and the editor already knows how to split it.
- **Applying the default schema after the setup.** The dedicated field would override a
  longer `search_path` written on purpose in the setup.
- **Letting "ignore errors" skip refusals.** Then a read-only connection's barrier would be
  one checkbox away.

## What would change this
- Several PostgreSQL databases under one connection (its own "Decide:" item) would make the
  default schema a pair (database, schema).
- If teams need a setup that writes on Production (an audit row per session), it would be a
  type safeguard of its own that only the personal layer can turn on, never the repository.
- If the allow list of session settings proves too short, entries are added there with a test
  that tries to get around each one.
