# ADR-006: Low-level drivers, not `sqlx`

- **Status:** Accepted
- **Date:** 2026-11-02

## Context
`sqlx` shines in applications with known queries (it checks them at compile time). A
database client runs arbitrary SQL and needs: OIDs and native types, the simple protocol
(several statements, everything as text), notices (`NOTICE`), cancellation and several
result sets.

## Decision
- PostgreSQL: `tokio-postgres`.
- MySQL/MariaDB: `mysql_async`.
- SQLite: `rusqlite` (synchronous, in its own thread; cancellation with `interrupt()`).

## Implementation (2026-11-02)
- **PostgreSQL:** TLS with `rustls` (`ring` backend) and libpq's `sslmode` modes:
  `disable`, `prefer` (default), `require`, `verify-ca`, `verify-full`. The trusted
  authorities are the file given in the connection or, if there is none, the system store.
  Cancellation uses the same TLS as the connection.
- **SQLite:** one thread per connection (an actor) with a bounded channel to the stream, so
  a long query neither blocks the runtime nor fills memory. Dynamic typing: each column's
  type comes from the declared type and the first batch; after that only lossless
  conversions are accepted and, otherwise, an explicit error (data is never lost silently).
  Text with invalid UTF-8 is treated as bytes. The file is not created if it does not
  exist, unless asked; `read_only` opens in native read-only mode (the basis for the MCP,
  ADR-008).

## Revisit if
Not expected.
