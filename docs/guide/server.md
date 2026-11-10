# Watching the server

The **Processes** button in the activity bar opens the process monitor. Pick a PostgreSQL,
MySQL, MariaDB or SQL Server connection on the left: the monitor opens a session of its own,
reusing the password of an open tab when there is one. Its three views, chosen at the top
right, answer "why is it stuck?":

- **Processes**: every session on the server, what it runs and for how long.
- **Locks**: who holds which lock, who waits, and who blocks whom.
- **Settings**: the server's configuration, with search.

Nothing here changes the server: every statement the monitor runs is a read, checked as one
before it runs. Auto refresh (Off, 5, 10 or 30 seconds) and Refresh apply to the view shown.

## Processes

The table lists each session with its user, host, database, command, time and running SQL,
filtered by text, state (active, idle, waiting) and database. Select one to see its SQL, copy
it or open it in an editor tab. **Cancel query** stops what it runs; **End connection** closes
it, rolling back its open transaction. Both ask first, are refused on a read-only connection,
and check that the session is still the one you selected before acting. SQL Server can only end
another session, not cancel its query alone.

## Locks

**Who blocks whom** shows each session that blocks others without waiting itself, with the
sessions waiting for it underneath, as deep as the chain goes. A deadlock the server has not
broken yet is marked where it closes. Under it, the table lists every lock: its session, the
object (a table, a transaction, a key…), the kind of resource, the mode, whether it is granted
or waiting, and for how long (since its transaction started, or since the wait began). Filter
it to waiting locks or to the sessions involved in a wait.

Select a session in the tree, or a lock in the table, to see the lock, the session's user,
host and SQL, who it waits for and who waits for it. **End connection** there ends the
blocking session with the same confirmation and checks as in the process view.

What each engine shows:

- **PostgreSQL**: every lock in `pg_locks` (except each transaction's lock on its own id) and
  the waits from `pg_blocking_pids`. Tables of other databases show as their number.
- **MySQL 8.0+**: InnoDB's locks (`performance_schema.data_locks`) and metadata locks
  (`performance_schema.metadata_locks`, so an `ALTER TABLE` waiting for a long transaction
  shows up). A session waiting for a metadata lock is paired with every other session holding
  one on the same object, as MySQL's own `sys.schema_table_lock_waits` does. It needs the
  `PROCESS` privilege and `SELECT` on `performance_schema`.
- **MariaDB**: only the InnoDB locks that block or wait (`information_schema.INNODB_LOCKS`);
  it needs `PROCESS`.
- **SQL Server**: `sys.dm_tran_locks` (without each session's shared lock on its database)
  and the waits from `sys.dm_os_waiting_tasks`. Keys and pages are named only in the
  connection's current database. It needs `VIEW SERVER STATE` (or `VIEW SERVER PERFORMANCE
  STATE`).

Without the permission the view shows the server's refusal.

## Settings

PostgreSQL's `pg_settings` (with its category, description and where each value comes from),
MySQL's and MariaDB's global variables and status counters, and SQL Server's configuration
options and server properties. Search by name, value or description, and filter by kind.
Select one to see its whole value and description, and copy the value. Settings are changed in
the editor or in the server's configuration, not here.

## Table statistics

A table's details tab (double-click Columns, Indexes… under a table in the explorer) shows its
size on disk, the table without its indexes, its indexes, and the server's row estimate, all
from the catalog's statistics: nothing is counted or scanned, so they can lag behind the data
(MySQL caches them; PostgreSQL estimates rows after `ANALYZE`). DuckDB only estimates rows, and
SQLite keeps none of these.
