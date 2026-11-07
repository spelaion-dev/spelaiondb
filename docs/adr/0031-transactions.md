# ADR-031: Transactions you can see and control: manual commit per tab, smart commit per type

- **Status:** Accepted (implemented)
- **Date:** 2026-11-07
- **Builds on:** ADR-028 (idle sessions), ADR-029 (connection types), ADR-014 (table edits),
  ADR-019 (agents' writes)

## Context
Every statement run from the editor commits as it runs. Against production there is no way to
run a few writes, look at the result and then decide, and nothing shows that a transaction is
open: a user who types `BEGIN` sees no sign of it, can close the tab or quit with it open, and a
lost session says nothing about the work it took with it. ADR-029 left DBeaver's auto-commit,
smart commit and "close idle transactions" as future work because there was no manual-commit
mode.

The drivers already know, without a query, whether a transaction is open: ADR-028's keep-alive
reads PostgreSQL's ReadyForQuery status, MySQL's `SERVER_STATUS_IN_TRANS` flag and SQL Server's
TDS transaction descriptor. SQLite says it with `sqlite3_get_autocommit`; DuckDB with the
transaction id of two consecutive statements.

## Decision
1. **The driver says the transaction state; the core never guesses it.** A new
   `Connection::transaction_state()` returns `Idle`, `Open` or `Failed` (an open transaction
   the engine will only roll back: PostgreSQL after an error, DuckDB after an error). It costs no
   round trip where the protocol carries it:
   - PostgreSQL: ReadyForQuery's status (`I`, `T`, `E`).
   - MySQL and MariaDB: `SERVER_STATUS_IN_TRANS` of the last OK packet.
   - SQL Server: the TDS transaction descriptor (the patched `in_transaction`), or
     `select @@trancount` after a cancel or an error left it uncertain.
   - SQLite: `sqlite3_get_autocommit`.
   - DuckDB: `select txid_current()` twice, in-process: the same id means a transaction is
     open; "transaction is aborted" means `Failed`.
   - SQLite on a server (ADR-018) cannot tell: it stays auto-commit only, and its tab says so.

   The keep-alive and the idle close (ADR-028, ADR-029) use the same answer.
2. **The state lives in the core session.** Each session has a `TransactionStatus`: whether
   auto-commit is on, whether its type asks for smart commit, whether smart commit switched it
   to manual, the transaction state and how many statements ran in the open transaction. It is
   read from the driver after every statement and every commit or rollback, so it is always
   what the server says (also after a `BEGIN`, `COMMIT` or DDL the user typed, or MySQL's
   implicit commits). The app only shows it and sends the user's clicks.
3. **Manual commit (auto-commit off) is the same on every engine:** before running a statement
   while no transaction is open, the core opens one with the engine's own `BEGIN` (`BEGIN
   TRANSACTION` on SQL Server and DuckDB, `START TRANSACTION` on MySQL). It is not sent
   - before transaction control the user typed (`BEGIN`, `COMMIT`, `ROLLBACK`, `SAVEPOINT`,
     `RELEASE`, `SET TRANSACTION`…): it is run as written;
   - before the few statements an engine refuses inside a transaction (PostgreSQL's `VACUUM`,
     `CREATE DATABASE`, `… CONCURRENTLY`, `ALTER SYSTEM`; SQL Server's `CREATE/ALTER/DROP
     DATABASE`, `BACKUP`, `RESTORE`; SQLite's `VACUUM`, `ATTACH`, `DETACH`): they run on their
     own, or fail with the engine's error when a transaction is already open.

   Reads open a transaction too, as in psql's `AUTOCOMMIT off`, JDBC and DBeaver: a function
   called from a `SELECT` may write, and manual mode promises that nothing is committed until
   the user commits. The engine's own session setting (`SET autocommit = 0`,
   `IMPLICIT_TRANSACTIONS`) is not used: PostgreSQL, SQLite and DuckDB have none, and one
   mechanism is tested once.
4. **Commit and Rollback** run the engine's `COMMIT` / `ROLLBACK` on the session, refused while
   a script is running on it. Turning auto-commit back on is refused while a transaction is
   open: the user commits or rolls back first (nothing is committed by a toggle). Turning it off
   sends nothing until the next statement. The core's own `BEGIN`, `COMMIT` and `ROLLBACK` are
   not the user's SQL and are not recorded in the query history.
5. **The badge tells the truth.** A statement that ends the open transaction without being
   `COMMIT` or `ROLLBACK` says so in the run's messages: "committed by this statement" (MySQL and
   MariaDB commit before DDL, and before a new `BEGIN`) or "rolled back by this error" (SQL
   Server with `XACT_ABORT`, a MySQL deadlock). On MySQL, `ROLLBACK` reports the server's warning
   when non-transactional tables (MyISAM) changed and could not be rolled back: MyISAM ignores
   transactions, and the app does not pretend otherwise.
6. **Smart commit is a connection type safeguard** (ADR-029's "only add up" rule): `smart_commit
   = true` on a type switches a session in auto-commit to manual right before the first
   statement that is not a read (the same classification `guard.rs` uses for "confirm data
   changes"), so that write runs in a transaction the user must commit. When that transaction
   ends (Commit, Rollback, or a statement that ends it) the session returns to auto-commit. It
   is **on for the built-in Production type**, settable on any type, and no layer can switch
   it off. A second setting, `manual_commit = true`, starts the type's sessions with auto-commit
   off (also only adds up). The user may still toggle auto-commit in a tab; smart commit stays
   on there, because it is the type's, not the tab's. Both are part of what a trusted repository
   pins (ADR-022); they are pinned only when on, so existing trust snapshots do not change.
7. **Closing asks first.** `Session::close_needs_confirmation()` asks the driver once more and is
   true while a transaction is open (a failed one has nothing to commit). The app asks before
   closing a tab, reconnecting or switching a tab's connection, and quitting; the explorer's
   own sessions never run user SQL. SDB-4's tab management uses the same check.
8. **A lost session with an open transaction says so.** The session keeps the last state it
   read; when it is lost (ADR-028), the tab's "Connection lost" adds that a transaction was open
   and that the server rolls back what was not committed when a session ends. Nothing is
   reconnected, re-run or re-opened (ADR-028). A reconnected tab starts from its type's default
   (auto-commit, or manual with `manual_commit`).
9. **Table edits (ADR-014) keep their own transaction.** They are always one transaction of
   their own, confirmed on Production, so smart commit and manual mode do not apply to them, and
   table data tabs have no auto-commit toggle. The core refuses to apply edits on a session with
   an open transaction rather than nest or commit someone else's work. Statements the core
   writes itself (edits, the explorer's object actions, paging, the process list) run as
   written, without the manual-mode `BEGIN`.
10. **Agents are untouched.** `spelaiondb mcp` reads and writes (ADR-019, ADR-020) use their own
    sessions, never an editor's, and their own transactions; the commit policy is applied only
    to editor sessions, by the app, like the keep-alive.

## Left out
- **Close idle transactions after N seconds** (DBeaver's setting): it rolls back the user's
  work, so it needs its own warning design. Tracked in the backlog.
- **Rollback to the last good statement on error** (psql's `ON_ERROR_ROLLBACK`): with savepoints
  around every statement; a later option.
- **Isolation levels** in the toolbar: `SET TRANSACTION` typed in the editor works.

## Alternatives
- **The engines' own auto-commit settings.** Natural on MySQL and SQL Server, impossible on
  PostgreSQL, SQLite and DuckDB; two mechanisms with different corner cases.
- **Opening transactions only for writes in manual mode.** Fewer "TX open" badges after a
  `SELECT`, but a `SELECT` that calls a writing function would commit, breaking the promise of
  manual mode. That is what smart commit is for.
- **Tracking the state by parsing statements.** Wrong after implicit commits, errors that roll
  back, procedures that commit, and `BEGIN` inside T-SQL batches; the drivers already know.
- **Committing when auto-commit is turned back on** (JDBC's behaviour). A toggle that commits is
  a surprise; asking for Commit or Rollback is explicit.

## What would change this
- If users find the badge after a plain `SELECT` in manual mode noisy, an option to open
  transactions only before writes (as smart commit does) could be added per tab.
- If an engine's state ever costs a round trip that matters (a long script of thousands of
  statements on DuckDB), the core could read it only when the script ends and before deciding a
  `BEGIN`.
