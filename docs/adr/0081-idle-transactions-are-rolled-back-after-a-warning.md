# ADR-081: Idle transactions are rolled back after a warning, as the connection type asks

- **Status:** Accepted (implemented)
- **Date:** 2026-11-25
- **Builds on:** ADR-028 (idle sessions), ADR-029 (connection types), ADR-031 (transactions),
  ADR-022 (repository trust), ADR-060 (lifecycles are explicit state types)

## Context
An editor tab that leaves a transaction open holds its locks on the server until someone
notices: a `BEGIN` typed and forgotten, a tab in manual commit (ADR-031) left before lunch, a
smart-commit write nobody committed. ADR-029 closes idle *sessions*, but never one with a
transaction open, so the sessions that hold locks are exactly the ones it leaves alone.
DBeaver's connection types have "close idle transactions after N seconds"; ADR-031 left it out
because it rolls back the user's work and needed its own warning design.

## Decision
1. **A connection type safeguard**, `close_idle_transaction_after = <seconds>`, with ADR-029's
   rule: it only adds up. The shortest time across the layers that define the type wins, no
   layer can switch it off, and absent or 0 means off. It is between 60 seconds and one day:
   shorter would roll back work the user is still reading the result of, before a warning could
   be read. It is part of what a trusted repository pins (ADR-022), only when set, so snapshots
   trusted before it do not change. No built-in type sets it.
2. **What "idle" means.** The clock runs only while the session's transaction is open, as the
   core last read it from the driver (ADR-031: `Open` or `Failed`; `Connection::transaction_state`,
   never guessed). It measures the time since the session's connection was last *used* by
   something the user did: a statement of a run, a page or more rows of a result, a commit, an
   edit applied, an explorer or editor read on that session. The core's own reads of the
   transaction state and ADR-028's keep-alives do not count. A session is **in use**, and is
   never touched, while:
   - a run is going, paused after an error waiting for the user, or being cancelled (the run
     state, SDB-370);
   - anything holds its connection (a statement whose rows are still being read, one of the
     core's reads);
   - it is lost or closed.

   Edits waiting to be applied in a grid are the page's, not the server's: they hold no lock,
   a rollback does not touch them, and they are applied later in their own transaction
   (ADR-014). They neither count as use nor stop the clock.
3. **The warning.** When the session has been idle for the time minus a lead (60 seconds, or
   half the time when that is shorter), the core marks the open transaction as warned, with the
   moment it will be rolled back. The tab shows it where the transaction badge is: "This
   transaction has been idle since …: it is rolled back at … unless the tab is used. Commit or
   roll it back now." Commit and Rollback stay where they are. Anything that uses the session
   (running a statement, Commit, Rollback) ends the warning, and the clock starts again from that
   use. There is no "keep it open" button: running a statement already does that, deliberately.
4. **What happens at the time: a rollback, not a closed session.** The core holds the session
   as Commit does (no run can start meanwhile), checks once more that nothing used it and asks
   the driver for the state; only when a transaction is still open does it run the engine's
   `ROLLBACK`, the statement Rollback runs (ADR-031). The session stays open with its other
   state (temporary tables, `SET` variables, the current schema): only what the transaction held
   is undone, which is what holds the locks. The tab then says, once, in its messages: "Rolled
   back after N minutes idle, as the connection type asks: M statements were undone." A session
   that smart commit had switched to manual returns to auto-commit, as after any rollback.
5. **When the rollback cannot be run.** If the `ROLLBACK` fails or the server does not answer
   in ADR-028's 30 seconds, the session is marked lost with that reason: the server rolls back
   what was not committed when the session ends (ADR-031's lost-session message), and the user
   reconnects as for any lost session. Nothing reconnects or re-runs by itself (ADR-028).
6. **One state, kept in the core (ADR-060).** The warning is part of the transaction's phase:
   `Open` and `Failed` carry an `IdleRollback` that is either not due or warned with the moment
   of the rollback. It is set and cleared only by the lifecycle's transitions, sent to the tab
   with the rest of the transaction status, and read there as its generated type. The rollback's
   notice is an event, sent once; it is not a state.
7. **Who uses it.** Only editor sessions that follow their transaction (ADR-031), started by the
   app like ADR-028's keep-alive and ADR-029's idle close. Engines that cannot tell whether a
   transaction is open, and servers whose `BEGIN` opens none, are never rolled back. Agents'
   sessions do not use it (ADR-031, point 10): their transactions are their own and short.

## Alternatives
- **Close the session instead of rolling back** (what DBeaver does): the server rolls back too,
  but the user also loses the session's state and must reconnect. A rollback frees the same
  locks and keeps the rest; closing stays the fallback when the rollback cannot be run.
- **Commit instead.** Never: nothing is committed behind the user's back (ADR-031).
- **Count keep-alives or the tab being open as use.** A tab open in the background is exactly
  the case this is for.
- **No warning, only the notice afterwards.** The work is lost without a chance to commit it;
  the warning is what makes the safeguard acceptable on Production.
- **A "keep it open" button.** It would make the safeguard a click to dismiss every few minutes;
  running a statement is the same deliberate act.
- **The server's own timeouts** (PostgreSQL's `idle_in_transaction_session_timeout`, MySQL's
  `wait_timeout`): they end the whole session, exist on some engines only, and are the DBA's to
  set. They still apply on top of this.

## What would change this
- If users lose work they meant to keep despite the warning, a system notification when the
  app is in the background (ADR-035's opt-in) is the next step, not a longer lead.
- If a team needs it on a type only for some engines, the setting could take an engine list; it
  does not now.
