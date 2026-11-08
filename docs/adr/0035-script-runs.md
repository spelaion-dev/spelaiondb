# ADR-035: Script runs that behave like a script runner: error modes, timeouts and server output

- **Status:** Accepted (implemented)
- **Date:** 2026-11-07
- **Builds on:** ADR-008 (splitting and classification in the core), ADR-028 (idle sessions),
  ADR-029 (connection types and their confirmations), ADR-031 (transactions)

## Context
A script always stopped at its first error and ended without a summary. Long migrations and seed
scripts need more control: go on past a statement that is allowed to fail, or decide each time;
a limit on how long a statement may run; a line that says how many statements ran and failed;
and the server's own output (PostgreSQL's notices reached the editor, SQL Server's `PRINT` and
MySQL's warnings did not). Going on after an error is not the same on every engine once a
transaction is open, and ADR-031 made that common: manual commit opens one before the first
statement, and smart commit before the first write.

## Decision
1. **Three error modes, run by the core.** `Session::run_script_with(script, ScriptOptions)`
   takes `OnError::Stop` (the default, as before), `Continue` or `Ask`. The run keeps splitting
   the script with the engine's dialect and running statements one by one; React only shows
   what it is told. A personal setting (Settings › Scripts, in the local store) gives the
   default; each editor tab can choose another for its runs.
2. **"Ask" is a state machine in the core with a ticket.** After a failed statement with
   statements left, the run stores a one-shot reply slot, yields `ErrorPaused { index, ticket,
   remaining }` and waits. `Session::answer_error(ticket, Stop | Skip | SkipAll)` resumes it;
   an answer with any other ticket, or a second answer, is refused (`AnswerError::NotWaiting`).
   `Skip` goes on and asks again at the next error; `SkipAll` turns the rest of the run into
   `Continue`. The pause also ends, as a stop, when the session is cancelled
   (`ScriptStop::Cancelled`), when its tab closes (`Session::stop_paused_run`, called by the
   app's `disconnect`), when the session is dropped, or when it is lost or idle-closed
   (ADR-028) (`ScriptStop::Closed`). While it waits the run still holds the session: Commit,
   Rollback and the auto-commit toggle stay refused (ADR-031's "busy"), so the answer applies
   to the transaction as it was when the question was asked.
3. **Going on must respect the transaction.** Whatever the mode, the run stops (no question
   asked) when:
   - the user cancelled: a cancel always ends the run. The session counts cancels, so a cancel
     that an engine reports as an ordinary error (PostgreSQL's `57014`) still stops it;
   - the failed statement left the transaction **aborted** (`TransactionState::Failed`): every
     later statement would fail with "current transaction is aborted", so the run stops with
     `ScriptStop::TransactionAborted` and says that only a rollback is left;
   - the failed statement **rolled back** the transaction it ran in (the state went from open
     to idle on a statement that is not `COMMIT`/`ROLLBACK`): going on would run the rest
     without the statements before it, in new transactions, so the run stops with
     `ScriptStop::TransactionRolledBack` (ADR-031's "rolled back by this error" notice is still
     shown).

   The state is what the driver says after the statement (ADR-031), not a guess from the error.
   A session that does not follow its transaction is still asked once whether one is aborted.
   Smart commit needs nothing special: its switch to manual happens before the write, so a
   failed first write is judged like any statement in manual commit, and when the transaction
   ends the session returns to auto-commit as before.

   What a failed statement does to an open transaction, per engine (tested in
   `crates/spelaion-core/tests/script_runs.rs` with a duplicate key in statement 3 of 5, in
   auto-commit and manual commit, in every mode):

   | Engine | Failed statement in a transaction | "Continue" / "Ask" in manual commit |
   |---|---|---|
   | PostgreSQL | The whole transaction is aborted | Stops: transaction aborted, roll back |
   | DuckDB | The whole transaction is aborted | Stops: transaction aborted, roll back |
   | MySQL, MariaDB | Only the statement is undone; a deadlock (1213) or a lock wait timeout with `innodb_rollback_on_timeout` rolls the transaction back | Goes on in the same transaction; stops if it was rolled back |
   | SQL Server | Statement-terminating errors undo the statement; `XACT_ABORT ON` and transaction-dooming errors roll back or doom it (`XACT_STATE() = -1` is read as aborted) | Goes on; stops if rolled back or doomed |
   | SQLite | Constraint errors undo the statement; I/O, full-disk and some busy errors roll the transaction back | Goes on; stops if rolled back |

   In auto-commit every statement is its own transaction, so "continue" simply goes on, on
   every engine. A `BEGIN` typed in the script puts the session in the same rules.
4. **The guard is not part of the mode.** `check_script` (ADR-029) is called by the app before
   every run whatever its mode, and refuses a run that skipped the confirmation: "continue on
   error" never runs a risky statement that was not confirmed.
5. **A statement timeout per connection and per run, by cancelling from the client.** A
   connection's `statement_timeout = <seconds>` (0 or absent: none; at most a day; for every
   engine, files included) applies to its editor runs; a tab can set its own for its runs (0:
   none). It counts from sending the statement to its last row. When it fires, the core uses the
   driver's existing cancel (PostgreSQL's cancel request, MySQL's `KILL QUERY`, SQL Server's TDS
   Attention through the patched tiberius, SQLite's and DuckDB's interrupt) and the statement
   fails with `timed_out`; the error mode then applies like any failure. It is a different
   setting from ADR-032's `connect_timeout`, which only covers connecting.
6. **The summary and the timing come from the core.** `ScriptFinished` carries a
   `ScriptSummary`: statements in the script, executed, succeeded, failed, not run, why it
   stopped, and the execution and fetch time summed over the statements. Each
   `StatementFinished` splits its time into execution (until the engine's first answer: its
   columns, row count or error) and fetch (reading the rows after it).
7. **Server output reaches the Messages tab for every engine that has any:**
   - PostgreSQL: notices, as before.
   - SQL Server: INFO tokens (`PRINT`, `RAISERROR` with severity 10 or less, "Changed database
     context…"). tiberius only logged them; the vendored copy now keeps them per connection
     (at most 1,000 per batch, then a count) and `Client::take_info_messages` hands them over,
     also when the batch fails before any result (see `vendor/tiberius/PATCHED.md`).
   - MySQL and MariaDB: warnings and notes. The OK packet of every statement counts them, so
     `SHOW WARNINGS LIMIT 100` is sent **only when the count is not zero**: no extra round trip
     for the statements that have none, which are almost all. The core's own `SHOW WARNINGS`
     after a `ROLLBACK` (ADR-031's MyISAM warning) is gone: the driver reports it.
   - SQLite and DuckDB have no server messages.
8. **An opt-in system notification** when a run that took longer than a threshold (30 s by
   default) finishes while the app is not in front (another window has the focus, it is
   minimised, or another tab is shown). It names the tab and counts what ran; it never contains
   SQL, data or error text. It is sent through `tauri-plugin-notification` from a Rust command
   (no JavaScript permission). Its Linux side (notify-rust, zbus 5) brings three crates that
   `cargo deny` reports as unmaintained (merged into zbus 6, not vulnerable); `deny.toml` lists
   them with that reason until notify-rust moves to zbus 6.

## Left out
- **Rollback to the last good statement** (psql's `ON_ERROR_ROLLBACK`, savepoints around each
  statement), which would let "continue" go on inside an aborted PostgreSQL transaction: already
  left for later by ADR-031.
- **Skipping ahead to the script's next `ROLLBACK`** after an aborted transaction: too clever to
  predict; the run stops and says why.
- **Agents.** `spelaiondb mcp` runs one statement per call (ADR-019, ADR-020); it keeps its own
  limits.

## Alternatives
- **Server-side timeouts** (`SET statement_timeout` on PostgreSQL, `max_execution_time` on
  MySQL). They change session settings the user can see and set too (`SHOW statement_timeout`,
  a `SET` in the script), MySQL's covers only `SELECT`, and SQL Server, SQLite and DuckDB have
  none. One client-side mechanism behaves the same on every engine and reuses the cancel that is
  already tested; a statement that returns as the cancel arrives just succeeds.
- **Letting the app drive the run** (sending the remaining statements one by one from React).
  It would move splitting and the transaction rules out of the core (ADR-002, ADR-008), and a
  closed tab would leave the decision nowhere.
- **Asking even after an aborted transaction**, with only "Stop" enabled. A question with one
  possible answer is noise; the run stops and the Messages tab says why.
- **`SHOW WARNINGS` after every MySQL statement.** One round trip each, for nothing in almost all
  cases; the warning count in the OK packet is free.

## What would change this
- If users want to go on inside an aborted PostgreSQL transaction, savepoints per statement
  (ADR-031's "left out") would let "continue" do it, at one round trip per statement.
- If a client-side timeout proves unreliable through slow tunnels (the cancel itself must travel
  the tunnel), PostgreSQL's server-side `statement_timeout` could be set for the run and
  restored after it.
