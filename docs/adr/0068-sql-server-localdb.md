# ADR-068: SQL Server LocalDB: the instance named in place of a host, started and reached by the plugin

- **Status:** Accepted (implemented)
- **Date:** 2026-11-23
- **Builds on:** ADR-005 (driver model), ADR-032 (connection test), ADR-041 and ADR-042 (engines
  as plugins), ADR-053 (read-only connections)

## Context
SQL Server Express LocalDB is the SQL Server that Visual Studio and many .NET projects use on a
developer's Windows machine. It is not a service listening on a port. Each instance listens
only on a named pipe of its own (`\\.\pipe\LOCALDB#…\tsql\query`, a new name each time it
starts) and is started on demand, either by Microsoft's `SqlLocalDB` program or by the first
client of Microsoft's own libraries. Its owner logs in with their Windows account: SQL logins
exist only if someone creates them.

The SQL Server plugin spoke TDS over TCP only, and logged in only with a SQL login, so it could
not reach LocalDB (SDB-312).

## Decision
1. **A connection names the instance in place of a host**, as SQL Server's own tools write it:
   `host = '(localdb)\MSSQLLocalDB'`, with `engine = "sqlserver"` and
   `engine_kind = "localdb"` from the picker. No new field, so the plugin protocol does not
   change: the plugin reads the host it already receives.
2. **The plugin starts the instance and finds its pipe.** It runs `SqlLocalDB start <name>`,
   which does nothing when the instance already runs, then reads the pipe from
   `SqlLocalDB info <name>`. That output is in Windows' language, so the plugin looks for the
   one value starting `np:\\.\pipe\`. It then speaks TDS through the pipe; tiberius takes any
   byte stream.
   - `SqlLocalDB.exe` is the one on the PATH, where its installer puts it. It runs without a
     shell and without a window, is killed if the connection gives up, and counts within the
     connection's `connect_timeout`.
   - Its only arguments are the subcommand and the instance name. The name is checked twice,
     when the configuration loads and in the plugin: letters, digits, spaces, `_`, `-` and `.`,
     up to 128 characters, never starting with `-`. Nothing else reaches the command line.
   - The plugin owns this, not the core: starting the instance is how SQL Server is reached,
     like opening its TCP socket. The core stays engine-agnostic, and the program is fixed by
     the engine, never named by the user.
3. **No user means the Windows account** (SSPI, tiberius's `winauth` on Windows), and only for
   LocalDB. A SQL login with a password still works if the instance has one. Windows
   authentication to SQL Servers on the network is another matter (Kerberos, names through
   tunnels) and is left out.
4. **Nothing of a network route applies.** A LocalDB connection loads with no tunnel,
   Kubernetes, port, `local_port`, `verify-ca`/`verify-full` or CA; the configuration refuses
   each, saying why, and the plugin refuses a forwarded address too. The login is still
   encrypted, with LocalDB's own certificate, which there is nothing to verify against.
5. **Windows only, and said so.** The picker lists "SQL Server LocalDB" on Windows. Elsewhere it
   is listed as "(Windows only)" and cannot be picked. A LocalDB connection in a shared
   repository still loads on macOS and Linux, and connecting there fails, saying LocalDB runs
   only on Windows.
6. **The connection test** (ADR-032) has no "reach host:port" step for it: there is no address
   to probe. Its login step starts the instance and logs in, and a failure there says which of
   the two failed.
7. **Everything after the login is SQL Server's**: the same plugin, dialect, catalog, read
   barrier (ADR-053), cancel (TDS attention, through the pipe) and agents' rules.

## Tests
- The plugin's `tests/localdb.rs` runs on Windows wherever `SqlLocalDB` is installed. GitHub's
  Windows runners have it, so the plugin's CI runs it. The test:
  - stops `MSSQLLocalDB` first, so the plugin is the one that starts it;
  - logs in with the Windows account, runs a batch and a rolled-back transaction, cancels a
    `WAITFOR` through the pipe, and opens a second connection;
  - checks the refusals: a bad name, a missing instance, a tunnel's address, and a certificate
    to verify.
- Without `SqlLocalDB` the test says so and stops, which does not count as tested. On Linux it
  checks the refusal.
- The configuration's and the connection test's rules are tested on every system.

## Alternatives
- **The core starts the instance:** it would need SQL Server's own program and its output in
  the core, and the plugin would still need the pipe.
- **LocalDB's API (`SqlUserInstance.dll`, `LocalDBStartInstance`):** no program to run, but
  native code through FFI and `unsafe`, which the plugin forbids. `SqlLocalDB` is the same API
  behind a program.
- **The user types the pipe's name:** it changes each time the instance starts.
- **A field of its own (`localdb_instance`):** clearer in the file, but the plugin protocol's
  `ConnectParams` would change for every plugin. `(localdb)\name` is what users already type
  in SQL Server's tools.

## What would change this
- If users need Windows authentication to SQL Servers on their network, an ADR of its own
  (Kerberos, NTLM, and how they work through a tunnel).
- If `SqlLocalDB` is not on the PATH for many installations, finding it through the registry
  key LocalDB's installer writes.
