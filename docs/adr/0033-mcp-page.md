# ADR-033: The MCP server page: read-only mode for every agent, compact output, and one-click setup of AI clients

- **Status:** Accepted
- **Date:** 2026-11-07
- **Builds on:** ADR-008 (MCP), ADR-019 (agents' writes), ADR-022 (repository trust),
  ADR-029 (connection types)
- **Amends:** ADR-008's append-only audit log: the user may clear all of it, leaving a record

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

Setting the server up is manual today: the user installs the CLI and edits each client's
configuration file by hand. Those files belong to the clients and hold the user's other
servers, so the app must change them without breaking anything and only with the user's say.

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

### Installing the server in AI clients
9. **Six clients, each with its own file,** found by the core (`spelaion-config::mcp_clients`):

   | Client | File |
   |---|---|
   | Claude Desktop | `%APPDATA%\Claude\claude_desktop_config.json` on Windows (the Microsoft Store version's private copy, `%LOCALAPPDATA%\Packages\Claude_*\LocalCache\Roaming\Claude\`, when it exists: that version reads it first), `~/Library/Application Support/Claude/` on macOS, `~/.config/Claude/` on Linux (unofficial builds) |
   | Claude Code | `~/.claude.json` (or `$CLAUDE_CONFIG_DIR/.claude.json`), its top-level `mcpServers`: the user scope, for every project |
   | Codex | `~/.codex/config.toml` (or `$CODEX_HOME`), `[mcp_servers.spelaiondb]` |
   | Cursor | `~/.cursor/mcp.json` |
   | Windsurf | `~/.codeium/windsurf/mcp_config.json` |
   | Antigravity | `~/.gemini/antigravity/mcp_config.json` |

   Every JSON client keeps its servers under `mcpServers`; our entry is `spelaiondb` with
   `command` (the binary's absolute path) and `args = ["mcp"]`, plus `"type": "stdio"` for
   Claude Code, which is what `claude mcp add --scope user` writes. The page also shows that
   command, and the manual configuration of the selected client to copy.
10. **Nothing is written without showing it first, and only our entry changes.**
    - The core computes the new file and a line diff of it; the app shows the diff and writes
      only on the user's click.
    - JSON files are edited as text: `serde_json` checks the file, a small scanner finds the
      spans of `mcpServers` and of our entry, and only those bytes change (our `command`,
      `args` and `type`; any other key the user added to our entry, such as `env`, stays).
      Indentation and line endings follow the file's. Rewriting the whole file through a
      parser would reorder or reformat other servers, and `~/.claude.json` is a large file
      Claude Code rewrites all the time.
    - Codex's TOML goes through `toml_edit`, which keeps comments and formatting.
    - A file that does not parse (including JSON with comments) is never written: the page
      says so and shows the manual configuration.
    - The change written is the change shown: the app sends back the diff's fingerprint, and
      the core re-reads the file, recomputes the change and refuses if either differs ("look
      at it again"). The file is written beside itself and moved over the old one, so a client
      never reads half of it.
    - The old file is kept as `<file>.spelaion-backup-<YYYYMMDD-HHMMSS>` (UTC; `-2`, `-3`… if
      taken), next to it. Backups are not cleaned up: they are small and the user's.
11. **Which `spelaiondb` the clients run:** the app looks for, in order, `SPELAION_CLI`
    (an explicit override), a `spelaiondb` binary next to its own executable, and the first one
    on the `PATH`. Without one, nothing can be installed and the page says how to get it.
    - **The installers ship the CLI next to the app** (Tauri's `externalBin`, set only in
      `tauri.release.conf.json`): the release workflow builds `spelaion-cli` for each target
      first. That is what makes one click enough on a clean machine. Local and CI builds do
      not bundle it, so they need no extra build step.
    - **A development build** finds `target/<profile>/spelaiondb` next to
      `spelaion-desktop` once `cargo build -p spelaion-cli` has run in the same target
      directory, or one on the `PATH`.
    - **An AppImage** runs from a mount that changes on every start, so its neighbour is never
      used: the `.deb` or the CLI on the `PATH` are the way on Linux.
    - Moving the app (or a new install path) leaves the old path in the clients' files; the
      card then says "Set up with another command" and offers to update it.
12. **Tests and the smoke test never touch the user's files:** `SPELAION_MCP_CLIENTS_HOME`
    points every client's file under a directory of their own.

### The activity log
13. **Every tool call is recorded,** not only queries and writes: `list_connections`,
    `list_objects`, `describe_table` and `search_schema` too, with a short description in place
    of a statement (`schema=public filter=ord`). Each entry now carries the **tool** and the
    **session**: an id made when a `spelaiondb mcp` process starts (its start time and process
    id), so the page groups calls per process. Two nullable columns added to the `audit` table by
    a migration; older entries have neither and belong to no session.
14. **The page reads it through the core:** filters (text in the statement, tool, status,
    session), counts, sessions (client, first and last call, calls, refused, failed) and exports
    are store queries and core functions; React holds only the rows on screen (500 at a time).
15. **Secrets are masked before anything leaves the core** (`spelaion_core::audit::redacted`):
    quoted literals after `PASSWORD`, `IDENTIFIED BY`, `SECRET`, `PWD` or `TOKEN`, and the
    password of URLs (`user:pass@host`). The log keeps what the agent sent (it is the user's own
    record on their computer); the page, the CSV and the JSON never show those parts. A heuristic:
    it catches what statements usually carry. The CSV is quoted and never starts a cell with a
    formula character (`=`, `+`, `-`, `@`), since statements come from agents.
16. **Clearing amends ADR-008's "append-only":** the user may empty the whole log, after a
    confirmation. The core does it in one transaction that lifts the delete trigger, deletes
    every entry, puts the trigger back and inserts a record saying when and how many entries
    went (status *cleared*). Nothing else can delete or change an entry: single entries cannot be
    removed or edited, from the app or from `spelaiondb`, and the record of the clearing stays.

### The page
17. **A chip icon in the activity bar, above Settings,** opens the page in place of the editor, with a short intro and its tabs: *Configuration* (one card per client: its file, whether it is set up, "Install configuration"; the selected client's manual configuration with a copy button, and `claude mcp add` for Claude Code), *Activity* (Events: time, connection, tool, statement, status and duration, with search, filters by tool and status, a count, CSV and JSON export, and Clear; Sessions: the calls per `spelaiondb mcp` process, opening into its events), *Security* (the default output, the read-only switch, and every connection with what agents can do there, as `agent_view` decides, and its type's safeguards in the app). The approval gate waits for SDB-38.

## Alternatives
- **The switch in `user.toml`:** visible and editable by hand, but the app would rewrite a file
  the user edits, and the server would have to re-read and re-validate it on every call.
- **A per-connection personal override** ("agents read-only on this one"): useful, and also
  tighten-only, but connection ids are only unique per workspace and the store has no key for
  that yet. Left for later (SDB-62); the global switch and the connection form cover today's
  needs.
- **Keeping Markdown as a third format:** it is what agents got until now, but it cannot be
  read back and costs more tokens than TOON; two formats are enough.
- **Rewriting client files through a JSON parser:** simpler, but it reformats and reorders the
  user's file (`~/.claude.json` is large and Claude Code's own); editing spans keeps every
  other byte.
- **Downloading the CLI on first use, or asking users to install it:** a download from the app
  needs a trusted source and a signature check of its own; shipping it in the installer, signed
  with the app's update artifacts, is simpler and works offline.
- **Pointing clients at the app's own executable with an `mcp` argument:** one binary fewer,
  but the GUI binary would then start (and link WebView) for every agent; the CLI stays small
  and headless (ADR-004).
- **CSV instead of TOON:** as compact, but without a header that says how many rows follow,
  and with quoting rules clients disagree on.

## Consequences
An agent that wrote through `execute_write` gets a refusal while the switch is on; nothing is
queued or retried. A read-only database user stays the only real guarantee (ADR-020).

## Revisit if
MCP clients commonly support elicitation (ADR-019's `"confirm"`): the approval gate (SDB-38)
may then sit next to this switch. If agents read TOON badly in practice, JSON stays the
default.
