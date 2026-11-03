# SpelunkDB

An open-source desktop database client: fast and lightweight, with first-class SSH tunnels
and a built-in MCP server for AI agents.

> **Status:** 0.2.0 preview. The drivers (PostgreSQL, MySQL/MariaDB and SQL Server with TLS,
> SQLite, DuckDB), the configuration, SSH tunnels, `spelunk mcp` and the desktop app work.
> `spelunk mcp` does not serve MySQL/MariaDB, SQL Server or DuckDB connections yet.

## Install

Download the installer for your system from the
[releases](https://github.com/petilolz/spelunkdb/releases):

- **Windows:** `SpelunkDB_<version>_x64-setup.exe` (current user, no admin rights) or the
  `.msi`. Not signed yet: SmartScreen may ask, "More info" → "Run anyway".
- **macOS:** the `.dmg` for Apple Silicon (`aarch64`) or Intel (`x64`). Not notarized yet:
  right-click → Open the first time.
- **Linux:** `.deb`, `.rpm` or AppImage.
- **CLI and MCP server:** the `spelunk-<version>-<target>` archives hold the `spelunk` binary.

Releases are built by CI from a version tag (`.github/workflows/release.yml`): bump the
version in `Cargo.toml` and `apps/desktop/src-tauri/tauri.conf.json`, write
`docs/releases/vX.Y.Z.md`, and push the tag `vX.Y.Z`.

## Build from source

With Rust, Node and the [Tauri prerequisites](https://tauri.app/start/prerequisites/):

```bash
python examples/demo/make-shop-db.py
```

```bash
cd apps/desktop && npm install && npm run tauri dev -- -- ../../examples/demo
```

`examples/demo` ships a local SQLite database and, if you start `testing/docker`, a direct
PostgreSQL, MySQL, MariaDB and SQL Server, and a PostgreSQL behind two SSH bastions with TLS.

## Adding connections

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

- **Save in:** *Project* writes `.spelunk/connections/<folder>/<name>.toml` in the
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
  file, and write its path on that server. SpelunkDB runs that server's `sqlite3` (3.37 or
  later, in its safe mode, which refuses commands that touch the server's files) through
  the tunnel: the file is never copied, and the server's own locks keep it safe while its
  application writes. The last hop must allow running commands, not only port forwarding.
  Cancelling a query ends that session (`sqlite3` stops when interrupted); reconnect to go
  on.

**Import from DBeaver…** (in the same window) reads DBeaver's workspace, or a folder you
choose, and lists its connections: PostgreSQL, MySQL, MariaDB, SQL Server, SQLite and DuckDB
ones can be imported,
with their folder, environment (DBeaver's connection type) and SSH tunnel (one SpelunkDB tunnel
per bastion and user, shared by the connections that use it). The user names and passwords
DBeaver saved are read from its credentials file; the passwords go to the system keychain only
if you tick that option, and are asked when connecting otherwise. A name already taken gets
` (DBeaver)` added; nothing existing is overwritten.

- **Kubernetes:** a connection can go through **Kubernetes** instead of an SSH tunnel, in
  its own tab of the form: pick a context of your kubeconfig, a namespace, the resource type
  (service, pod or deployment), its name and its port, every list read from `kubectl` as you
  go. SpelunkDB runs `kubectl port-forward` with your kubeconfig (so EKS, GKE or AKS
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

## Settings

The gear button at the bottom of the sidebar (or `Ctrl+,`) opens **Settings** in its own tab:

- **Appearance:** light, dark or the system theme.
- **Language:** English, Spanish or the system language. Translations are bundled with the
  application and work offline. Database and SSH server messages are shown as received.
- **History:** what query history stores and where to turn it off.

Preferences are stored in the personal `state.db` and apply to every workspace.

## Schema explorer

Expanding a connection in the sidebar lists its schemas (databases in MySQL), tables and
views with their columns, primary key and estimated rows (**Count rows** in the right-click
menu gets the exact number). Double-click a table or view (or **View rows**) to open its
**table data tab**: pages of 100 to 5,000 rows with ◀ ▶, a `WHERE` and an `ORDER BY` you type,
header clicks that sort in the server, and **Count** for the exact number of matching rows.
The core writes `SELECT * … LIMIT n OFFSET m` and checks it is still one plain SELECT of that
table, so nothing typed in the filter can run another statement or load the whole table.
**Open as SQL** puts `SELECT * … LIMIT 100` in a new SQL tab instead. Right-click it to generate a `SELECT` with every column
or, for tables, an `INSERT`, an `UPDATE` (on the primary key) or a `DELETE`. The core writes
them with the engine's quoting and `:name` placeholders for the values; nothing runs until
you replace them and run it.

### Editing data

In a table data tab of a table with a primary key, double-click a cell (or **F2**) to change
it; the right-click menu also has **Set NULL**, **Delete row** (or the **Del** key) and
**Revert changes**, and **Add row** opens a form (empty fields take the column default).
Changes stay pending, drawn in the grid (edited cells highlighted, deleted rows struck
through), until **Apply** or **Discard**; paging and filtering wait meanwhile. The core
writes the `UPDATE`/`DELETE` statements by primary key (read from the database, with the key
values of the loaded rows) and the `INSERT`s, checks each typed value against its column type,
and applies everything in one transaction: if a statement does not affect exactly one row
(the row changed or vanished meanwhile), nothing is applied. Production connections show the
statements for confirmation first, and read-only connections refuse. Tables without a primary
key are read-only (ADR-014).

## Running SQL

For MySQL, MariaDB and SQL Server, the database selector beside the connection chooses the
default database for that query tab. It shows the current database after `USE` statements too; other
tabs and the saved connection are unchanged. Connect first to load the databases available
to the user. Double-click a MySQL or MariaDB database in the connected explorer to open a
query tab already connected to it. Queries and table tabs opened from the connected explorer
use their own sessions, without asking again for the password already provided there.

**Ctrl+Enter** runs the selection or, without one, the statement under the cursor, which is
highlighted for a moment. The core finds it with the same dialect splitter that runs scripts
(quotes, comments, dollar-quoted bodies and MySQL `DELIMITER` included), so what is
highlighted is exactly what runs. **Run** in the editor's side toolbar does the same.
**Ctrl+Shift+Enter** or **Run script** runs the whole script,
statement by statement, stopping at the first error. **Shift+Alt+F** or **Format** formats
the selection or the whole script in the connection's dialect (one undo step).

`:name` placeholders (like those in generated SQL) ask for their values before running; the
same name gets the same value everywhere, and the tab remembers them for the next run. The
core writes them in as literals with the engine's quoting: numbers, `NULL`, `TRUE` and
`FALSE` as they are, anything else as text. Strings, comments and PostgreSQL `::casts` are
never mistaken for placeholders.

Before anything runs, the core looks for statements that are easy to regret: `DELETE` or
`UPDATE` without `WHERE` (also inside a `WITH`), `DROP` and `TRUNCATE`, and, on connections
whose environment is `prod`/`production`, every statement that is not a read. They are listed
in a confirmation dialog where **Cancel** has the focus; nothing runs until you choose
**Run anyway**. This is a safety net against slips; read-only mode remains the barrier for
connections that must never change.

### Keyboard shortcuts

| Keys | Action |
| --- | --- |
| Ctrl+Enter | Run the selection or the statement under the cursor |
| Ctrl+Shift+Enter | Run the whole script |
| Shift+Alt+F | Format the selection or the script |
| Ctrl+S | Save the SQL as a named query |
| Ctrl+T / Ctrl+W | New query tab / close the tab (its connection and results are freed) |
| Ctrl+Shift+T | Reopen the last closed tab with its SQL |
| Ctrl+Tab, Ctrl+Shift+Tab | Next / previous tab (also Ctrl+PageDown / Ctrl+PageUp) |
| Ctrl+, | Settings |
| Ctrl+C, Ctrl+Shift+C | Copy the grid selection (with headers) |
| Enter, Esc | Open / close the cell viewer |
| Ctrl+Shift+J | Jump to a column of the result by name |

## Working with results

Select cells in the result grid by clicking and dragging, with Shift and the arrow keys, by
clicking row numbers (whole rows) or with Ctrl+A. **Ctrl+C** copies the selection as
tab-separated text for spreadsheets and **Ctrl+Shift+C** adds the column names. The
right-click menu also copies as CSV, JSON (numbers stay numbers, JSON columns are embedded)
or a single SQL `INSERT` with all selected rows and the engine's quoting, as a Markdown table or as an SQL
`IN` list (`(1, 2, 3)`, repeated values once). The core formats the rows from the
typed data; up to 100,000 rows can be copied at once.

**Export ▾** writes the whole result to a file (CSV with a byte order mark so Excel reads
accents, TSV, JSON, SQL `INSERT` or a Markdown table). The core writes it in chunks to a temporary file that is
renamed at the end, so it works with millions of rows and never leaves half-written files.

Drag the edge of a column header to resize the column; double-click the edge to fit it to
its name and the visible values.

Clicking a column header sorts the loaded rows ascending, then descending, then back to the
server's order (also in the right-click menu). The core sorts by the typed values (numbers and
decimals by value, dates by date) with NULLs last, and copies and exports follow that order.
**Filter rows** above the grid keeps the rows where some cell contains the text (ignoring
case), also filtered in the core: the status line shows "15 of 50,000 rows", sorting works on
what is left, and copies and exports take only those rows. **Esc** clears it.

Column filters work like SQLyog's: right-click a cell, **Filter ▸** offers `= 'value'`,
`<> 'value'`, `>`, `<`, `LIKE '%value%'`, `NOT LIKE`, `IS NULL` and `IS NOT NULL` for that cell,
plus **Custom filter…** (any column, operator and value) and **Reset filters**. Conditions add
up (AND), show above the grid with a × each, and follow SQL: a NULL only matches `IS NULL`.
Numbers compare by value and text ignores case. They run in the core on the loaded rows, so
they also work where a `WHERE` cannot go, such as `SHOW FULL PROCESSLIST` filtered by
`Command <> 'Sleep'`.

**Enter** or a double-click opens the cell viewer beside the grid with the full value of the
active cell: JSON pretty-printed, binary values as a hex dump and long text with its line
breaks. **Row** switches to every column of the active row, one under the other (click a
field to see that value in full). The arrow keys move through cells while it stays open;
**Esc** closes it.

## Saved queries

**Ctrl+S** (or **Save** in the editor toolbar) saves the tab's SQL under a name, together with
its connection; saving again offers the same name, and an existing name is replaced.
**Saved queries** in the sidebar lists them with a search box: **Open** puts one in a new tab
(without connecting or running it) and **Delete** asks for a second click. They live in the
local store on this computer, per repository, or for every repository when saved outside one.

## Query history

Open **Query history** in the sidebar to search executed SQL or connection IDs and open a
statement in a new, disconnected editor tab. Opening it never runs it. History records each
executed statement with its connection, start time, duration, returned row count and error
(including cancellations), and survives app restarts.

**Save query history**, in the "⋯" menu next to each connection in the sidebar, controls
recording for that connection. Disable it
before running SQL containing passwords or other secrets: SQL text and database errors are
stored as written. The preference is personal, takes effect across tabs and survives app
restarts. Disabling it keeps existing entries; it does not erase previously saved SQL.

History and preferences live in `state.db` in the personal configuration directory, outside
the repository. Project connections are separated by project; personal connections share
their history across folders. This setting controls desktop history; the MCP audit log is
separate.

## For AI agents (MCP)

`spelunk mcp` gives an agent **read-only** access to the connections you mark with
`[mcp] exposed = true`. Install the binary and add it to Claude Code:

```bash
cargo install --path apps/cli
```

```bash
claude mcp add spelunkdb -- spelunk mcp
```

Every query is recorded: `spelunk audit`.

- Architecture: [`docs/architecture.md`](docs/architecture.md)
- Decisions: [`docs/adr/`](docs/adr/README.md)
- Phase 0 experiments: [`spikes/`](spikes/README.md)

License: Apache-2.0.
