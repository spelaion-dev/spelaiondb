# ADR-049: Backup and restore through the tunnel: the vendors' own tools, started by the core with an argument list and the password in their environment

- **Status:** Accepted
- **Date:** 2026-11-11
- **Builds on:** ADR-004 (no daemon), ADR-007 (SSH tunnels), ADR-008 (read-only mode),
  ADR-015 (Kubernetes), ADR-027 (proxies), ADR-029 (connection types), ADR-034 (streaming
  exports), ADR-035 (script runs), ADR-036 (SQL files)

## Context
Backing up a database behind a bastion means opening a shell there, or setting up a tunnel by
hand and pointing `pg_dump` at it. SpelaionDB already opens the tunnel (SSH hops, a proxy, or a
Kubernetes forward), so it can hand the tool its local end.

Until now the app never ran a program of the user's machine on behalf of a connection, except
`kubectl` (ADR-015) and the remote `sqlite3` (ADR-018). Dumping is a place where the vendors'
own tools are right on edge cases an own dumper gets wrong (sequences, ownership, extensions,
definers, every type's text), so they are worth running. Running them is a change of direction:
it needs rules for how they are found, started and stopped, and for the password.

## Decision
1. **The vendors' tools, found by the core.** PostgreSQL uses `pg_dump`, `pg_restore` (custom,
   directory and tar archives) and `psql` (plain SQL files); MySQL and MariaDB use `mysqldump`
   and `mysql`, or MariaDB's own `mariadb-dump` and `mariadb`. `spelaion-core::backup::tools`
   looks for them in the folder the user chose (a personal setting), or else on `PATH` and in the
   places their installers use (`C:\Program Files\PostgreSQL\<n>\bin`, MySQL Server, MariaDB and
   Workbench folders, Postgres.app, Homebrew's `libpq` and `mysql-client`, Debian's
   `/usr/lib/postgresql/<n>/bin`), newest version first. The tool's `--version` is read and shown,
   and a `pg_dump` older than the server's major version is reported before it runs (it would
   refuse).
2. **Through the tunnel's local end.** `open::open_route` opens the connection's route without a
   database session: the shared SSH tunnel (hops, a proxy, or a proxy alone, ADR-027), or the
   Kubernetes forward, from the same `TunnelPool` as the connection's sessions, so an open
   connection's tunnel is reused. The tool connects to `127.0.0.1:<local port>`. A connection
   with neither connects to its own host and port. PostgreSQL's tools get a libpq connection
   string with `host` = the real host name and `hostaddr` = the local end, so `verify-full` still
   checks the real name; MySQL's tools cannot do that, so `verify-full` through a tunnel becomes
   `VERIFY_CA` there and the log says so.
3. **The password goes in the tool's environment, nowhere else.** `PGPASSWORD` for PostgreSQL's
   tools and `MYSQL_PWD` for MySQL's and MariaDB's (both read it; no file is written). It is never
   an argument, and the argument list the dialog and the log show is the one that runs. Every log
   line passes through the connection test's `Redactor` (ADR-032), which knows the connection's
   and its tunnel's secrets, before it leaves the core. Tests check that the password is in
   neither the arguments nor the log.
4. **An argument list, never a shell.** `tokio::process::Command` with each argument as its own
   string; no `sh -c`, no `cmd /c`. Extra arguments the user types are split by the core
   (whitespace, single and double quotes) into separate arguments. On Windows the tool starts
   without a console window. A cancel kills the process; dropping the run kills it too.
5. **The core owns the files.** A dump's output is the tool's standard output, written by the core
   to `<name>.part` and renamed when the tool exits successfully (PostgreSQL's directory format is
   the one exception: `pg_dump --file` writes the folder, also as `.part`), so a cancel or a
   failure leaves nothing behind, as in ADR-034. A restore's input is fed by the core to the
   tool's standard input (except a directory archive), which gives progress as bytes read of the
   file's size. The output folder and a file name pattern (`{database}`, `{connection}`,
   `{timestamp}`, `{date}`) are the dialog's, rendered by the core into a name safe on every
   system. "Remove DEFINER" for `mysqldump` (which has no such option) is done on that stream:
   `DEFINER=…` clauses are removed from every line that is not a row of data.
6. **Restore and running a file are writes.** A read-only connection (ADR-008) refuses them in the
   core, before any program starts, and the core runs them only when the request says they were
   confirmed: the dialog is the confirmation, every time, whatever the connection's type
   (ADR-029). A dump is a read and runs on read-only connections.
7. **What the vendor's client may do while restoring.** `psql` and `mysql` run the backslash
   commands of the file they read, `\!` (a shell command) included. Where the client can turn that
   off, the core does: `--skip-system-command` (MySQL 8.0.40, 8.4.3, 9.1 and later) and
   `--sandbox` (MariaDB 10.5.25, 10.6.18, 10.11.8, 11.4.2 and later). Where it cannot (every
   `psql`, older `mysql`), the confirmation says that the file is run as written by that client
   and should be one the user trusts.
8. **SQLite and DuckDB use their own commands**, in a session of their own: `VACUUM INTO` for a
   SQLite copy, DuckDB's `EXPORT DATABASE` / `IMPORT DATABASE` for a folder. No external tool.
9. **Running an `.sql` file goes through our own session**, never a tool: the file is read with
   ADR-036's decoding, split by the engine's dialect, checked by `guard.rs`'s `check_script` and
   read-only mode like an editor run, and run statement by statement in a session of its own
   (never a tab's, ADR-031), with progress as statements done of the total, the elapsed time and
   cancel. Its statements do not go to the query history: the file is the record.
10. **Not in scope:** `pg_dumpall` (roles, tablespaces), maintenance commands (`VACUUM`,
    `ANALYZE`, `CHECK`/`REPAIR`/`OPTIMIZE TABLE`) and scheduled backups (no daemon, ADR-004).
    Agents (MCP) get none of this.

## Consequences
- A backup needs the client tools on the user's machine; the dialog says where it looked when
  it finds none, and lets the user choose the folder. An own plain-SQL dumper stays the
  alternative if users ask for backups without them.
- Nothing is added to the driver contract (ADR-041/042): the tools are started by the core, which
  only needs the resolved connection and the route.
- Tests run the round trips through the two-hop tunnel of `testing/docker` with the tools of the
  official images the test services already use (`docker run`, so the machine needs none), or with
  local tools in `SPELAION_TEST_DB_TOOLS`.
