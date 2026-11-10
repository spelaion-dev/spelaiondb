# Adding connections

The activity bar gives access to connections, query history, saved queries and settings.
Use the connections icon to show or hide the explorer. Drag its right edge to resize it;
double-click the edge to reset its width. The focused edge can also be moved with the arrow keys.

The **+** button at the top of the sidebar opens **Add connection**: pick a database from the
catalog (grouped as relational, analytical and NoSQL; engines without a driver yet are shown
as *coming soon*), fill in its form, **Test connection** and **Save**. You can also paste a
connection URL (`postgres://user:pass@host:5432/db?sslmode=require`, `mysql://…`,
`mariadb://…`, `sqlserver://…`, their `jdbc:` forms, `sqlite:path` or `duckdb:path`): it
picks the engine and fills host,
port, database, user, SSL mode and the password field (which is saved like a typed one).

- **Save in:** *Project* writes `.spelaion/connections/<folder>/<name>.toml` in the
  repository, shared through git; *Personal* keeps it in your personal directory.
- **Password:** saved in the system keychain, asked every time, or read from an environment
  variable. It is never written to the connection file.
- **SSH:** choose a tunnel, or create and edit one with **New tunnel…** / **Edit tunnel…**:
  the hops in order (the last one reaches the database), each with its host, user and
  authentication (SSH agent, including Pageant, a private key file, a password or
  keyboard-interactive 2FA). **Test tunnel** opens every hop. Hop passwords go to the
  keychain or are asked when connecting; key paths in project tunnels are stored relative
  to the repository or as `~/…`, so they work for the whole team. A tunnel in use by a
  connection cannot be deleted.
- **SQLite and DuckDB:** pick an existing file with **Browse…**, or create an empty
  database with **New database…** (it never replaces an existing file).
- **A SQLite file on a server:** in the SSH tab, choose a tunnel whose last hop holds the
  file, and write its path on that server. SpelaionDB runs that server's `sqlite3` (3.37 or
  later, in its safe mode, which refuses commands that touch the server's files) through
  the tunnel: the file is never copied, and the server's own locks keep it safe while its
  application writes. The last hop must allow running commands, not only port forwarding.
  Cancelling a query ends that session (`sqlite3` stops when interrupted); reconnect to go
  on.
- **Kubernetes:** a connection can go through **Kubernetes** instead of an SSH tunnel, in
  its own tab of the form: pick a context of your kubeconfig, a namespace, the resource type
  (service, pod or deployment), its name and its port, every list read from `kubectl` as you
  go. SpelaionDB runs `kubectl port-forward` with your kubeconfig (so EKS, GKE or AKS
  authentication works as in your terminal). Connections to the same resource and port share
  one `kubectl`, stopped with the last one; if it exits, the connection shows `kubectl`'s
  message and is not restarted on its own. In the file it is a `[kubernetes]` section:

  ```toml
  [kubernetes]
  context = "prod-eks"      # default: the current context
  namespace = "shop"        # default: the context's namespace
  resource = "svc/postgres" # svc/…, pod/… or deployment/…; `port` is its port
  ```

Testing a connection writes nothing. Saving never overwrites an existing connection.

The **⋯** menu next to each connection has **Edit…** and **Delete…**. Editing keeps the same
file: only the values you change are rewritten, so comments and settings the form does not
show (such as `mcp.max_rows`) survive. Deleting asks for confirmation, removes the file and
the password saved in the keychain, and leaves open tabs without a connection.

To share connections and tunnels through a git repository, see
[Sharing connections with a team](team.md).

## Importing from other clients

**Import from DBeaver…** (in the same window) reads DBeaver's workspace, or a folder you
choose, and lists its connections: PostgreSQL, MySQL, MariaDB, SQL Server, SQLite and DuckDB
ones can be imported,
with their folder, environment (DBeaver's connection type) and SSH tunnel (one SpelaionDB tunnel
per bastion and user, shared by the connections that use it). The user names and passwords
DBeaver saved are read from its credentials file; the passwords go to the system keychain only
if you tick that option, and are asked when connecting otherwise. A name already taken gets
` (DBeaver)` added; nothing existing is overwritten.

Since 0.8.0 the same window also imports from DataGrip (and the other JetBrains IDEs),
TablePlus, HeidiSQL, MySQL Workbench 8.0, pgAdmin and Beekeeper Studio: see the
[0.8.0 release notes](../releases/v0.8.0.md).
