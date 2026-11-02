# SpelunkDB

An open-source desktop database client: fast and lightweight, with first-class SSH tunnels
and a built-in MCP server for AI agents.

> **Status:** v0.1 under construction. The drivers (PostgreSQL and MySQL/MariaDB with TLS,
> SQLite), the configuration, SSH tunnels, `spelunk mcp` and a first version of the desktop
> app work. `spelunk mcp` does not serve MySQL/MariaDB connections yet.

## Try the app

With Rust, Node and the [Tauri prerequisites](https://tauri.app/start/prerequisites/):

```bash
python examples/demo/make-shop-db.py
```

```bash
cd apps/desktop && npm install && npm run tauri dev -- -- ../../examples/demo
```

`examples/demo` ships a local SQLite database and, if you start `testing/docker`, a direct
PostgreSQL, MySQL and MariaDB, and a PostgreSQL behind two SSH bastions with TLS.

## Adding connections

The **+** button at the top of the sidebar opens **Add connection**: pick a database from the
catalog (grouped as relational, analytical and NoSQL; engines without a driver yet are shown
as *coming soon*), fill in its form, **Test connection** and **Save**.

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

## Working with results

Select cells in the result grid by clicking and dragging, with Shift and the arrow keys, by
clicking row numbers (whole rows) or with Ctrl+A. **Ctrl+C** copies the selection as
tab-separated text for spreadsheets and **Ctrl+Shift+C** adds the column names. The
right-click menu also copies as CSV, JSON (numbers stay numbers, JSON columns are embedded)
or SQL `INSERT` statements with the engine's quoting. The core formats the rows from the
typed data; up to 100,000 rows can be copied at once.

**Export ▾** writes the whole result to a file (CSV with a byte order mark so Excel reads
accents, TSV, JSON or SQL `INSERT`). The core writes it in chunks to a temporary file that is
renamed at the end, so it works with millions of rows and never leaves half-written files.

**Enter** or a double-click opens the cell viewer beside the grid with the full value of the
active cell: JSON pretty-printed, binary values as a hex dump and long text with its line
breaks. The arrow keys move through cells while it stays open; **Esc** closes it.

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
