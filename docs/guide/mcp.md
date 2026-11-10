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
