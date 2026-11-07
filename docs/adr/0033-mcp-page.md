# ADR-033: The MCP server page: read-only mode for every agent, and compact output

- **Status:** Accepted
- **Date:** 2026-11-07
- **Builds on:** ADR-008 (MCP), ADR-019 (agents' writes), ADR-022 (repository trust),
  ADR-029 (connection types)

## Context
The app gets a page of its own for the MCP server (`spelaiondb mcp`): installing it in AI
clients, its activity, and its security. Two security questions need an answer before the
page can offer them:

- Users want one switch that makes **every** agent read-only, for a while or for good,
  without editing each connection. Writes are decided per connection today
  (`[mcp] writes = "allow"`, ADR-019), and a connection may come from a trusted repository
  (ADR-022) that the user does not edit.
- Query results go to the model as Markdown tables. A table repeats nothing per row, but its
  padding and separators cost tokens, and nothing in a cell is quoted, so a client cannot read
  it back reliably. Formats made for models, such as TOON (Token-Oriented Object Notation),
  name the columns once and write one line per row.

## Decision
### Read-only mode for every agent
1. **A personal setting, `mcp.read_only`, in the local store** (`state.db`, the
   `personal_settings` table), next to the audit log the MCP server already needs to serve
   anything. Off by default, so nothing changes until the user turns it on. The app's MCP page
   switches it; `spelaiondb mcp --read-only` does the same for one server, for client
   configurations that want it for good.
2. **It can only take writes away.** When it is on, `execute_write` is refused on every
   connection, with a message saying the user made every agent read-only. When it is off, each
   connection decides as before: `read_only`, then `[mcp] writes` (ADR-019). Nothing in a
   connection file, personal or a trusted repository's, and nothing about its type, Production
   included, can override it; it never gives writes to a connection that does not allow them.
   Reads (`run_query`, `explain_query`) are unchanged: they were always read-only.
3. **One decision, in the core:** `spelaion_core::agent_access(file, all_read_only)` returns
   `ReadWrite` or `ReadOnly(reason)` (`write.rs`, a CODEOWNERS file). The MCP server refuses
   writes with it and `list_connections` tells the agent before it tries; the app's page shows
   the same answer per connection.
4. **Read on every call, failing closed.** `spelaiondb mcp` reads the setting before each write
   and each `list_connections`, so turning it on takes effect at once in servers that AI
   clients keep running for hours. If the store cannot be read, or holds a value other than
   `on` or `off`, every agent is read-only. The database itself refuses other values (a
   trigger, like the theme's), whatever statement writes them.
5. **Not in the repository, not in `user.toml`.** A repository could only make it stricter,
   but a team's wish to keep agents read-only is already expressed per connection
   (`writes = "deny"`) and checked by `spelaiondb check`. `user.toml` is configuration the
   user edits by hand; this is a switch the app flips, read by a process that already holds
   the store open.

### Compact output
6. **Query rows come as JSON or TOON**, for `run_query` and `explain_query`:
   - **JSON** (the default): one object with `columns` (name and type), `rows` (one array of
     values per row), `row_count`, `more_rows` and `elapsed_ms`, plus a `note` when not every
     row fits. Numbers and booleans are JSON literals; decimals are strings, so no digit is lost
     to a client that reads numbers as floats.
   - **TOON**: the columns as a small table, then the rows as one tabular array whose header
     names the fields once (`rows[3]{id,name}:`), one comma-separated line per row. Strings are
     bare unless something could read them as another thing (a delimiter, a colon, quotes,
     `true`/`false`/`null`, something that looks like a number, leading or trailing spaces);
     then they are quoted with TOON's five escapes. A repeated column name gets `_2`, `_3`….
   - Both keep ADR-008's limits (200 characters per cell, about 60 KB per response) and say when
     rows were left out. Both quote every cell, so data cannot end a row, add one or pass for a
     field: returned data is untrusted (ADR-008).
7. **The default is a personal setting,** `mcp.output_format` (`json` or `toon`), in the store
   like the switch and chosen in the page's Security tab; **a call can override it** with
   `output_format`. Markdown tables are no longer produced: JSON carries the same information
   and can be read back.
8. Other tools (`list_connections`, `list_objects`, `describe_table`, `search_schema`) keep
   their short text answers: they are lists, not tables of data.

## Alternatives
- **The switch in `user.toml`:** visible and editable by hand, but the app would rewrite a file
  the user edits, and the server would have to re-read and re-validate it on every call.
- **A per-connection personal override** ("agents read-only on this one"): useful, and also
  tighten-only, but connection ids are only unique per workspace and the store has no key for
  that yet. Left for later (SDB-62); the global switch and the connection form cover today's
  needs.
- **Keeping Markdown as a third format:** it is what agents got until now, but it cannot be
  read back and costs more tokens than TOON; two formats are enough.
- **CSV instead of TOON:** as compact, but without a header that says how many rows follow,
  and with quoting rules clients disagree on.

## Consequences
An agent that wrote through `execute_write` gets a refusal while the switch is on; nothing is
queued or retried. A read-only database user stays the only real guarantee (ADR-020).

## Revisit if
MCP clients commonly support elicitation (ADR-019's `"confirm"`): the approval gate (SDB-38)
may then sit next to this switch. If agents read TOON badly in practice, JSON stays the
default.
