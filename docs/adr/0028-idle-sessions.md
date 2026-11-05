# ADR-028: Idle database sessions: keep them alive, notice when one dies, reconnect only on a click

- **Status:** Accepted (implemented)
- **Date:** 2026-11-05
- **Builds on:** ADR-007 (in-process SSH), ADR-005 (driver model)

## Context
A connection left open overnight shows connection errors in the morning. What happens today:

- **SSH tunnels already send keep-alives** (every 15 s, closed as down after 3 unanswered;
  configurable per tunnel since this change). The session between this computer and the first
  bastion is therefore not dropped by a NAT while the computer is awake.
- **Nothing keeps the database session itself alive.** Through a tunnel, the TCP connection
  from the last bastion to the database carries no traffic while the editor is idle; a firewall,
  NAT or load balancer between them (AWS NLB after 350 s, Azure after 4 minutes, many corporate
  firewalls after an hour) forgets it without telling anyone. Servers close idle sessions too:
  MySQL and MariaDB's `wait_timeout` is 8 hours by default, exactly one night, and poolers such
  as pgbouncer have their own limits.
- **Direct connections have no TCP keepalive:** neither our PostgreSQL driver nor the SQL Server
  one sets it, and `mysql_async` leaves it off.
- **A dead session is noticed only by the next query**, which either fails at once (the server
  closed it) or hangs until TCP gives up (packets silently dropped: minutes). The tab still says
  "connected" and every following query fails the same way until the user picks the connection
  again. When the computer sleeps, the SSH keep-alives notice the dead tunnel about 45 s after
  waking, but nothing tells the tab.

## Decision (recommended)
1. **A database keep-alive per connection**, like DBeaver's "Keep-Alive" setting: when a session
   has been idle for `keepalive = <seconds>` (connection file; 0 or absent turns it off; the
   form suggests 300), the core runs the engine's cheapest no-op (`SELECT 1`; MySQL's
   `COM_PING`) on it, inside the session actor so it never overlaps a user's statement.
   - It is never sent while a transaction is open, so it does not keep locks or an
     `idle_in_transaction_session_timeout` alive for someone who forgot a `BEGIN`.
   - It is not a re-run: it is not the user's SQL, it is not recorded in the query history,
     and it changes nothing.
   - Agents' sessions (`spelaiondb mcp`) do not use it: they are short-lived.
2. **TCP keepalive on direct connections** (every driver's socket; 60 s idle, as libpq's
   default where the OS allows it), so a NAT in front of the database does not drop them.
3. **A dead session is shown as dead.** A connection error from the driver, a failed keep-alive
   or the tunnel going down marks the tab "connection lost" with the reason. The next run asks
   to reconnect, and reconnecting opens a new session (and tunnel, if needed) **without running
   anything**: the user runs the statement again. Nothing is ever re-run automatically
   (AGENTS.md).

## Implementation (2026-11-05)
- Drivers: `Connection::keep_alive()` returns `Alive` or `InTransaction`, and
  `Connection::is_closed()` says whether the driver already knows the connection is gone.
  PostgreSQL sends `select now() <> statement_timestamp()` (both clocks agree only on the
  first statement of a transaction, so it says whether one is open; a failed transaction
  answers 25P02, also open). MySQL and MariaDB read `SERVER_STATUS_IN_TRANS` from the last
  OK packet and send nothing when it is set, otherwise `COM_PING`. A plain `SELECT 1` with
  autocommit off opens no transaction there; touching an InnoDB table does. SQL Server sends
  `select @@trancount`. SQLite and DuckDB have none.
- TCP keepalive after 60 s idle: `keepalives_idle` in tokio-postgres (it waited two hours),
  `tcp_keepalive` in `mysql_async`, and `socket2` on SQL Server's socket.
- `spelaion-core::health`: every use of a session's connection goes through `Tracked`, which
  notes the last use and marks the session `Lost` on a connection error. SQLSTATE class 08
  and PostgreSQL's 57P01/57P02/57P05 count too: they arrive as ordinary errors before the
  driver sees the close. The keep-alive loop runs only after a whole interval without use
  and with nothing running. After `InTransaction` it waits until the session is used again.
  It counts the session lost after 30 s without an answer. A tunnel going down marks its
  sessions lost.
- Connection files take `keepalive = <seconds>`. The form fills in 300 for new connections,
  and imports from DBeaver get 300 too. The desktop starts the loop for editor sessions only
  (`spelaiondb mcp` does not). A lost session sends `SessionLost` to the UI. The tab then shows
  "Connection lost" with a Reconnect button, and Run explains instead of running. Reconnecting
  opens a new session and runs nothing.
- Tests:
  - MySQL and MariaDB with `wait_timeout = 3`: the session with keep-alives survives, the one
    without is lost.
  - `pg_stat_activity` shows keep-alives stop inside a transaction and resume after it.
  - A terminated backend is noticed by the keep-alive, and a statement that ends its own
    session marks it lost.
  - The smoke test covers the lost tab and the reconnect.

## Alternatives
- **Only TCP keepalive everywhere.** Cheap, but through a tunnel it only covers the local hop
  to 127.0.0.1, not the bastion-to-database leg, and it does nothing against `wait_timeout`.
- **Reconnect silently before the next query.** What some clients do; it hides the loss of
  session state (temporary tables, `SET` variables, an open transaction that was rolled back),
  and re-running the user's statement on a new session breaks the rule above.
- **Close idle connections after a while and reopen on demand** (DBeaver's "close idle
  connection" default of 30 minutes). Saves server resources but loses session state every
  time; it can be added later as an option.

## What would change this
- If DBAs object to keep-alive queries in their logs or to sessions that never time out, the
  default should be off and only the detection (part 3) kept on.
- If an engine's keep-alive turns out to cost something (a pooler that counts it as activity
  and keeps a server connection busy), that engine gets TCP keepalive only.
