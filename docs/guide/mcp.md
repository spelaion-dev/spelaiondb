# For AI agents (MCP)

`spelaiondb mcp` gives an agent access to the connections you mark with
`[mcp] exposed = true` ("Expose to AI agents" in the connection form). Agents only read,
unless you choose "Agents can: Read and write" (`[mcp] writes = "allow"`, ADR-019): then the
`execute_write` tool runs one statement at a time in its own transaction: data changes and
`CREATE`/`ALTER` of tables, views, indexes, schemas and sequences. `DROP`, `TRUNCATE`,
`DELETE`/`UPDATE` without `WHERE`, procedures, permissions, settings, transaction control and
anything that reaches files or other servers are always refused, and read-only connections
never let agents write. Every engine is served (PostgreSQL, MySQL, MariaDB, SQL Server,
SQLite and DuckDB), each behind its own read-only barrier (ADR-020); for DuckDB, agents never
reach files or the network. A read-only database user is still the safest choice for agents
that only read. One switch makes every agent read-only, whatever each connection allows
(ADR-033; `spelaiondb mcp --read-only` does the same for one server). Query rows come back as
JSON or as compact TOON (`output_format`, or your default). The app's **MCP server** page
(the chip icon above Settings) sets it up in Claude Desktop, Claude Code, Codex, Cursor,
Windsurf and Antigravity with one click: it shows the change to the client's file first, keeps
a backup, and touches only its own entry. The installers ship the `spelaiondb` command next to
the app. Without the app, install the binary and add it to Claude Code:

```bash
cargo install --path apps/cli
```

```bash
claude mcp add spelaiondb -- spelaiondb mcp
```

Every tool call, run or refused, is recorded on your computer: the MCP page's Activity tab
shows, filters, exports and clears it (secrets masked), and so does `spelaiondb audit`.

## Why a call was refused or failed

When a tool call is refused or fails, the agent gets the same sentence as before and, in the
result's `structuredContent`, a stable code and whether running the same call again makes
sense (ADR-060, SDB-374):

```json
{ "code": "session_lost", "retry": true }
```

The code carries no data: no secret, no SQL, no path. The server never runs anything again on
its own; `retry` only tells the agent whether it may. The audit log records the same code.

| Code | What happened | `retry` |
|---|---|---|
| `session_lost` | The session was lost during the call (the server ended it, the network or a tunnel went down). The next call opens a new one. | `true` for reads; `false` for a write, which may have been committed |
| `connection_unavailable` | The connection could not be opened. | `true` when it may answer next time (the network, a tunnel, a timeout); `false` when only you can fix it (a password asked interactively, SSH authentication, a secret, the session setup) |
| `not_exposed` | The connection does not exist or is not exposed to agents, or the configuration has a problem and exposes nothing. The same code in every case, so it never tells an agent that a hidden connection exists. | `false` |
| `not_trusted` | An SSH host key you have not accepted, one that changed, or a repository change you have not reviewed. | `false` |
| `read_only` | A write on a connection agents may only read (your switch, its local restriction, the connection, its `[mcp] writes`, its engine). | `false` |
| `refused_by_guard` | A barrier refused the statement before it was sent: a write through `run_query`, a statement `execute_write` never runs, a plan limit. | `false` |
| `engine_without_barrier` | The engine or server has no read-only barrier agents can rely on (SDB-169). | `false` |
| `plugin_missing` | The engine's plugin is not installed. | `false` |
| `cancelled` | The statement was cancelled. | `true` (a write is only cancelled before its commit, and rolled back) |
| `timed_out` | The statement exceeded the time limit; it has to be narrowed first. | `false` |
| `statement_failed` | The database refused or failed the statement. | `false` |
| `invalid_arguments` | The call's arguments do not go together (a plan page without a plan, `sql` and `plan_id` together). | `false` |
| `internal` | Something went wrong inside SpelaionDB. | `false` |

A failed write says `retry: true` only when its transaction is known to be rolled back.

`list_connections` also gives each visible connection's state as structured content:

```json
{
  "all_agents_read_only": false,
  "repository_not_trusted": false,
  "connections": [
    { "id": "~/shop", "engine": "postgres", "access": "read_write" },
    { "id": "~/prod", "engine": "postgres", "access": "read_only", "read_only_because": "writes_denied" }
  ]
}
```

`read_only_because` is one of `all_agents`, `connection_override`, `connection`,
`writes_denied`, `confirm_unsupported` and `engine_without_writes`. Hidden connections are
never listed.
