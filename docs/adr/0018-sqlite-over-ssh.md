# ADR-018: SQLite files on a server, through SSH

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
Many small services keep their data in a SQLite file on a server. Users want to browse and
edit it without copying it down and up again. SQLite has no server and no network protocol:
it is a file read by a library inside the program, so an SSH tunnel (port forwarding, ADR-007)
has nothing to forward to. Today a SQLite connection with a tunnel is a configuration error.

Three ways exist:
1. Run the `sqlite3` program **on the server**, through an SSH command, and talk to it over
   that channel.
2. Mount the remote folder (SSHFS, SFTP) and open the file locally.
3. Put a server in front of the file (libSQL's `sqld`, rqlite) and tunnel to its port.

## Decision
**Option 1: the server's own `sqlite3`, through an exec channel on the tunnel's last hop.**

- **Configuration:** `engine = "sqlite"` with `tunnel = "…"` means *the file is on the last
  hop*: `database` is its path there (absolute, or relative to the SSH user's home). An
  optional `sqlite3 = "/usr/local/bin/sqlite3"` names the program (default: `sqlite3` on the
  remote PATH). The combination is invalid today, so no existing file changes meaning.
- **Transport:** `spelunk-ssh` gains `Tunnel::exec(command)`, an SSH session channel on the
  last hop with its stdin and stdout, next to the port forwards it already opens. Host keys,
  hop authentication, `Interaction` dialogs and the `TunnelPool` are the same as for any
  tunnel; a changed fingerprint still blocks (ADR-007).
- **The driver contract stays free of SSH:** `ConnectParams` gets an optional
  `remote: Arc<dyn RemoteCommand>` (a trait in `spelunk-driver-api` that starts a process and
  returns its byte streams). The core builds it from the tunnel; the driver only sees streams.
  Kubernetes (ADR-015) can provide the same trait later with `kubectl exec -i` into a pod.
- **The process:** one per connection, started as
  `[ -f '<path>' ] || …; exec sqlite3 -safe -batch -header -cmd '.mode quote'
  -cmd '.changes on' -cmd '.timeout 5000' '<path>' 2>&1` (with `-readonly` for read-only
  connections). Every argument is quoted for the remote POSIX shell; the path is data, never
  shell syntax. A missing file is refused (`sqlite3` would create it, and a mistyped path
  must not leave an empty database). `.timeout 5000` waits for another writer's lock, like
  the local driver. It lives as long as the connection, so transactions and temporary
  tables work as with the local driver. Servers whose SSH shell is not POSIX (Windows) are
  not supported.
- **Reading results:** `quote` mode writes every value as an SQL literal (`1`, `1.5`,
  `'text'`, `X'00ff'`, `NULL`), which keeps SQLite's storage classes, unlike JSON. Types per
  column follow the local driver's rules for dynamic typing (ADR-006), from the first batch.
  `.changes on` gives the rows affected, which editing needs (ADR-014). After each statement
  the driver prints a marker with a random value it chose, so it knows where a statement's
  output ends; errors come in the same stream (`2>&1`), told from values by their first
  characters, and a text value with line breaks is read while its quote is open. The shell
  flushes after each statement. A query that returns no rows prints no header: its columns
  are not known, and it shows as "0 rows" without a result.
- **Safety, the important part:** the `sqlite3` shell is more than SQL: its dot-commands run
  shell commands (`.shell`, `.system`), write files (`.output`, `.once`) and load extensions,
  and functions such as `writefile()` and `edit()` reach the server's filesystem. So:
  - `-safe` is required (it refuses those commands and functions, and ends `sqlite3` when
    one is tried: the session says so). It exists since SQLite 3.37 (2021): an older
    `sqlite3` is refused with a clear message, never used without it.
  - The driver sends only statements from the core's splitter, each ending with `;`, and
    refuses one that starts with `.` (or `#`) once its leading comments are skipped, as the
    shell skips them; the statement is sent from its first word. Any later line that starts
    with `.` or `#` outside a quote or comment is written with a space before it (still the
    same SQL; a value is never changed): user text never reaches the shell as a dot-command.
  - Repository connections need a trusted repository (ADR-009): they run a program on a
    server, a stronger permission than forwarding a port.
  - Not served by `spelunk mcp`, like the other engines without a read-only barrier of
    their own.
- **Cancel:** an SSH `signal` request (`INT`) on the channel. In batch mode `sqlite3`
  interrupts the statement and then **ends** (it only carries on in its interactive mode,
  which echoes its input and cannot be read reliably): the statement is reported as
  cancelled and the session as ended (SQLite rolls back what was open); the user
  reconnects. Nothing is started or run again on its own.
- **Explorer:** the same catalog queries as the local driver (`sqlite_schema`,
  `pragma table_info`…), sent through the channel.

## Alternatives
- **Mounting the folder (SSHFS/SFTP) and opening it locally:** rejected. SQLite's locks do
  not hold over network filesystems; with the server's application writing at the same time
  the file can be corrupted. The SQLite documentation warns against it.
- **A server in front (`sqld`, rqlite):** sound, but someone must install and run it on the
  server. A libSQL driver may still come later for those who have one.
- **Uploading our own small agent to the server:** full control over the protocol, but
  copying executables to someone's server is intrusive and needs per-platform builds.
- **A separate engine name (`sqlite-ssh`):** clearer in the file, but the local and remote
  cases share everything except the transport; the tunnel already says "remote".

## Known limits
- Cancelling ends the session (see above).
- A query with no rows shows no columns.
- A huge result is read as it arrives; while the app does not take it, the channel waits,
  and so may other forwards on the same SSH session.
- `-safe` blocks `ATTACH` of other files, which is also what keeps the file boundary.
- Tested against `testing/docker/sqlitehost` (Alpine, SQLite 3.45) through two hops; a last
  hop that only forwards ports says so.

## Revisit if
`sqlite3` older than 3.37 turns out to be common on users' servers, or a libSQL/`sqld`
server becomes the usual way to serve SQLite.
