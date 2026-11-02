# ADR-011: Local storage in SQLite

- **Status:** Accepted
- **Date:** 2026-11-02

## Decision
- A local SQLite database (`rusqlite`) in WAL mode: the app and `spelunk mcp` write at the
  same time.
- Contents: history (with FTS5), snippets, MCP audit log, per-connection schema cache and
  UI state.
- Versioned migrations from day one.
- History may contain secrets (`CREATE USER … PASSWORD`): it can be disabled per
  connection and is never part of the shared configuration.
- Schema cache invalidated by manual refresh, after DDL run from the app, and by TTL.

## Revisit if
Not expected.
