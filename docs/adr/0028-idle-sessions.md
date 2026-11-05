# ADR-028: Idle database sessions: keep them alive, notice when one dies, reconnect only on a click

- **Status:** Proposed (waiting for a decision before it is built)
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
