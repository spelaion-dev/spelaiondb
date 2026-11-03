# ADR-019: Agents may write, per connection

- **Status:** Accepted
- **Date:** 2026-11-03
- **Supersedes:** layer 6 of ADR-008 (writes)

## Context
ADR-008 made `spelunk mcp` read-only and planned writes for later, with a per-connection
policy, human confirmation in the app and production always read-only. Users want agents to
change data on the connections they choose, production included when they decide so,
without confirming each write. An agent can still make mistakes, and what it reads may
carry instructions planted by someone (prompt injection), so writes keep barriers of their
own.

## Decision
- **Per connection, chosen by the user:** `[mcp] writes = "allow"` (in the form: "Agents
  can: read and write") or `"deny"` (the default: read only). It applies to production
  connections too: there is no special case for `env = "prod"`; the form warns when a
  production connection lets agents write. `"confirm"` (ask before each write) stays in the
  file format for later and is refused for now with a clear message. A connection marked
  `read_only` never lets agents write, whatever `mcp.writes` says.
- **A tool of its own:** `execute_write`, marked destructive. `run_query` and
  `explain_query` stay read-only (ADR-008's layers, unchanged): an agent never writes by
  querying. Reads and writes use separate sessions, so reads keep the native read-only
  connection where the engine has one.
- **What a write may be (checked in the core, `write.rs`):**
  - exactly one statement, which the parser understands (what it cannot read is refused);
  - not a read (those go through `run_query`);
  - never destructive: `DROP`, `TRUNCATE`, and `DELETE`/`UPDATE` without `WHERE`, also inside
    other statements;
  - never transaction control (`BEGIN`, `COMMIT`, `ROLLBACK`, savepoints): each write runs in
    a transaction of its own;
  - never other files: `ATTACH`/`DETACH`, `COPY`, `INSTALL`/`LOAD`.
- **How it runs:** in its own transaction, committed only if the statement succeeds, rolled
  back otherwise; with the same time limit as reads (`statement_timeout` in PostgreSQL, a
  client timer and a cancel elsewhere). The answer gives the rows affected.
- **Audit:** every write, executed, refused or failed, goes to the append-only audit log with
  its statement and rows affected.
- **Engines:** those the MCP already serves (PostgreSQL and SQLite, local or on a server).
  The others join the MCP with their own read barriers, and their writes with them
  (ADR-020, which also turns the kinds of write into an allow list).

## Alternatives
- **Confirm each write in the app:** the safest, but `spelunk mcp` runs without the app
  (ADR-004), and MCP clients that can ask the user (elicitation) are not yet common. Kept
  as `"confirm"` for later.
- **Production always read-only (ADR-008):** dropped at the user's request; the form's
  warning and the barriers above remain.
- **Letting `run_query` write when allowed:** one tool fewer, but the read-only guarantee of
  `run_query` would depend on configuration; a separate tool keeps it absolute.

## Revisit if
MCP clients commonly support elicitation (then `"confirm"`), or agents need statements the
barriers refuse (then a narrower exception, not a wider door).
