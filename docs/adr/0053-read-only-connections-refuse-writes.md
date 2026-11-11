# ADR-053: A read-only connection refuses writes from the editor: a check in the core before anything runs, and the engine's own read-only mode

- **Status:** Accepted
- **Date:** 2026-11-11
- **Builds on:** ADR-008 (the read-only classifier), ADR-020 (each engine's read barrier for
  agents), ADR-029 (connection types: safeguards only add up), ADR-037 (session setup and its
  read-only check)

## Context
A connection file has `read_only = true`, and the explorer, table edits, object actions,
imports, restores and agents already refuse to write on it. The editor did not: on SQLite and
DuckDB the file is opened read-only, so the engine refused, but on PostgreSQL, MySQL, MariaDB and
SQL Server the drivers never read the flag, and an `UPDATE` typed in an editor tab ran and
committed. A user who marked a production connection read-only to look at it safely was not
protected where it mattered most.

The pieces to fix it already exist: `classify_read_only` (ADR-008) decides what is a read for
agents, `classify_session_setup` (ADR-037) adds the session settings and `USE` that may run where
the read-only barrier applies, and PostgreSQL, MySQL and MariaDB have a read-only mode of their
own for a whole session.

## Decision
1. **The core refuses, before a script starts, every statement that is not:**
   - a read, as `classify_read_only` decides;
   - a session setting or `USE`, as `classify_session_setup` allows (same allow list, same
     plain values);
   - plain transaction control: `BEGIN` / `START TRANSACTION` with at most an isolation level and
     `READ ONLY`, `COMMIT`, `ROLLBACK` (to a savepoint too), `SAVEPOINT`, `RELEASE` and `SET
     TRANSACTION` with an isolation level or `READ ONLY`.

   Anything else is refused, and so are `READ WRITE` in any form (`BEGIN READ WRITE`, `SET
   SESSION CHARACTERISTICS AS TRANSACTION READ WRITE`, `SET default_transaction_read_only`,
   `RESET ALL`), MySQL's executable comments (`/*! … */`, which the server runs and the parser
   skips) and whatever does not parse. A T-SQL batch may hold several statements: each must
   pass. The check is `classify_read_only_connection` in `classify.rs`, applied to every
   statement of the script by `check_read_only` in `guard.rs`.
2. **Nothing of a refused script runs**, not even the statements before the refused one, and
   no confirmation overrides it: "run anyway" answers the guard's questions (ADR-029), not this.
   The app's run goes through `check_run` (next to `check_script`), which refuses first and
   asks nothing (`run_risks` lists no question for a script that will be refused). The error
   names the statement, its position and why, and says the connection is read-only.
3. **The session refuses it again.** `open_connection` marks the session of a read-only
   connection, and the session's user runs (scripts, query pages, plans, the history's runs, in
   editor tabs and the explorer alike) refuse a script the check refuses, before sending
   anything, whatever their caller checked. The core's own statements (setup, catalog, pages it
   writes) do not go through it: each of those paths has its own read-only rule.
4. **The engine's own read-only mode is set when the session opens**, before the session setup
   (ADR-037) and before the session is handed out: `SET SESSION CHARACTERISTICS AS TRANSACTION
   READ ONLY` on PostgreSQL, `SET SESSION TRANSACTION READ ONLY` on MySQL and MariaDB. Every
   transaction of the session is then read-only, so a function that writes behind a `SELECT`
   (`nextval`, a user function) fails in the engine. If it cannot be set, the session does not
   open. SQLite and DuckDB keep opening the file read-only. **SQL Server has no such mode**: the
   core's check is the barrier there, and a function cannot change data in T-SQL.
5. A read-only database user is still the only real guarantee, as ADR-008 says, and the
   documentation keeps saying so.

## Alternatives
- **Only the engines' read-only mode.** SQL Server has none, and on the others a user can turn
  it off with one statement (`SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE`).
- **Only the core's check.** A function that writes looks like a read to any parser; the
  engine's mode stops it where the engine has one.
- **Refusing only the statements that write and running the rest of the script.** A script is
  often a unit (read, then write what was read); running half of it on a connection the user
  marked read-only is a surprise. Refusing all of it is the same rule as the guard's.
- **Checking in the app (React or the Tauri command).** AGENTS.md puts every security decision
  in the core; the session's own check also covers the core's other callers.

## Consequences
- Some reads the parser cannot classify are refused on a read-only connection: SQLite's
  `PRAGMA table_info(t)`, `DESCRIBE t`, T-SQL's `DECLARE`, `COPY … TO STDOUT`, statements
  sqlparser does not know. The explorer shows the same information.
- A read-only connection's sessions are read-only for the core's own work too: on PostgreSQL,
  MySQL and MariaDB anything that would write fails in the engine, which every such path
  already refused before.

## What would change this
- SQL Server gains a read-only session or transaction mode: it is set there too.
- If users need a read the parser refuses (a pragma, `DESCRIBE`), it is added to an allow list
  for read-only connections with a test that tries to get around it, never by loosening
  `classify_read_only` for agents.
