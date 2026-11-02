# SpelunkDB

An open-source desktop database client: fast and lightweight, with first-class SSH tunnels
and a built-in MCP server for AI agents.

> **Status:** v0.1 under construction. The drivers (PostgreSQL with TLS, SQLite), the
> configuration, SSH tunnels, `spelunk mcp` and a first version of the desktop app work.

## Try the app

With Rust, Node and the [Tauri prerequisites](https://tauri.app/start/prerequisites/):

```bash
python examples/demo/make-shop-db.py
```

```bash
cd apps/desktop && npm install && npm run tauri dev -- -- ../../examples/demo
```

`examples/demo` ships a local SQLite database and, if you start `testing/docker`, a direct
PostgreSQL and another one behind two SSH bastions with TLS.

The language selector in the tab bar offers English, Spanish and the system language.
Translations are bundled with the application and work offline; the choice is stored in the
personal `state.db` and applies to every workspace. Database and SSH server messages are
shown as received.

## Query history

Open **Query history** in the sidebar to search executed SQL or connection IDs and open a
statement in a new, disconnected editor tab. Opening it never runs it. History records each
executed statement with its connection, start time, duration, returned row count and error
(including cancellations), and survives app restarts.

**Save history** in the editor toolbar controls recording for that connection. Disable it
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
