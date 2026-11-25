# ADR-086: A savepoint per statement rolls back only the failed one, when the user asks

- **Status:** Accepted
- **Date:** 2026-11-25
- **Supersedes:** ADR-035's "Left out: rollback to the last good statement" and, for runs that
  ask for it, its point 3's stop at a transaction the failed statement aborted
- **Builds on:** ADR-031 (transactions), ADR-035 (script runs), ADR-060 (the run's state is the
  core's)

## Context
With manual commit (ADR-031) a script runs inside one transaction. On PostgreSQL a failed
statement aborts that whole transaction: every later statement fails with "current transaction
is aborted", so ADR-035 stops a run that would go on ("continue" or "ask") and says only a
rollback is left. The statements that ran before it are lost with the rollback. psql's
`ON_ERROR_ROLLBACK` answers this by setting a savepoint before each statement and rolling back
to it when the statement fails: the transaction survives without the failed statement. It costs
one more round trip per statement, and two when it fails.

## Decision
1. **Opt-in, off by default:** "In a transaction, roll back only the failed statement", a
   personal setting in Settings › Scripts (`script.statement_savepoints` in the local store,
   next to the default error mode). The core reads it for every editor run
   (`ScriptOptions::statement_savepoints`); the page never decides which statements are wrapped.
   It is not a per-tab option: what it changes is the transaction's behaviour, not the run's
   mode.
2. **The core wraps a statement in its own savepoint** (`spelaion-core`'s run loop, `session.rs`)
   when all of these hold:
   - the run is the user's (an editor run; never the core's own reads, pages or edits);
   - the session follows its transaction and one is open before the statement: the core's
     manual-commit `BEGIN`, smart commit's, or the user's own `BEGIN`;
   - the statement is not transaction control: a user's `BEGIN`, `COMMIT`, `ROLLBACK`,
     `ROLLBACK TO SAVEPOINT`, `SAVEPOINT` or `RELEASE` runs as written, so the user's own
     savepoints keep working (rolling back to one of theirs would take the core's with it);
   - the engine's errors abort the whole transaction and it keeps savepoints inside one, and the
     server reached keeps them too (`transaction::statement_savepoint`, `keeps_savepoints`).

   Before the statement the core sends `SAVEPOINT spelaion_statement`. If the statement fails,
   it sends `ROLLBACK TO SAVEPOINT spelaion_statement` and then releases it, and the Messages tab
   says "Only this statement was rolled back: the transaction goes on without it." If it
   succeeds, the core sends `RELEASE SAVEPOINT spelaion_statement`. Each waits for a cancel still
   on its way (SDB-266). A savepoint that cannot be set leaves the statement to run as before; a
   rollback to it that fails leaves the transaction as the statement left it.
3. **Then ADR-035's rules apply unchanged,** to the state the driver reads after the core's
   savepoint statements: a transaction rolled back to the savepoint is open, so "continue" goes on,
   "ask" asks, and "stop" stops at the failed statement with the transaction still usable, and
   Commit keeps what ran. A cancel still ends the run. A failure the savepoint could not undo still
   stops as `TransactionAborted` or `TransactionRolledBack`.
4. **The core's savepoints are its own statements:** they are not statements of the run, are not
   counted in its summary or the transaction's statement count, and never reach the history,
   which records only the user's statements.
5. **Per engine:**

   | Engine | With the setting on |
   |---|---|
   | PostgreSQL, and CockroachDB and YugabyteDB reached through its protocol | A savepoint per statement: a failed statement is rolled back alone |
   | RisingWave, Materialize, CrateDB, unchecked PostgreSQL forks | As before (ADR-035): their savepoints are not known to work, and a refused `SAVEPOINT` would itself abort the transaction |
   | DuckDB | As before (ADR-035): the bundled DuckDB has no `SAVEPOINT` (`tests/script_runs.rs` checks it), so a failed statement still aborts the transaction and the run stops |
   | MySQL, MariaDB, SQL Server, SQLite | As before: they already undo only the failed statement (ADR-035's table), so no savepoint is needed |

## Consequences
- On PostgreSQL a long migration in manual commit can go past a statement that is allowed to fail
  and still be committed as one transaction, at one more round trip per statement while the
  setting is on.
- DuckDB keeps stopping: the card that asked for this expected it to go on, which needs savepoints
  DuckDB does not have. It is revisited when DuckDB adds them.

## Alternatives
- **On by default:** every statement of every run on PostgreSQL would pay the round trip, for a
  case most runs never meet.
- **A per-tab option next to the error mode:** it would multiply the choices in the run toolbar;
  the setting describes how the user wants transactions to behave, which is personal.
- **Skipping to the script's next `ROLLBACK` after an aborted transaction:** still too clever to
  predict (ADR-035).
- **Re-running the statements before the failed one in a new transaction:** never; nothing the user
  ran is re-run on its own (`AGENTS.md`).

## Revisit if
DuckDB adds savepoints, or another engine whose errors abort the whole transaction is added.
