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
- **MySQL/MariaDB (2026-11-03):** `mysql_async` with rustls (`ring`), one actor task per
  connection. Statements use the text protocol, so values arrive as the server writes them;
  integers, floats, dates and timestamps become typed Arrow columns, `BIGINT UNSIGNED` stays
  exact decimal text and `TIME` (a duration up to 838 hours) is an interval. Temporal columns
  are decided with the first batch: legacy zero dates (`0000-00-00`) keep the column as text,
  and a later one fails with a `CAST` hint. Errors keep their SQLSTATE (`28000` = access
  denied); syntax errors get a position by locating MySQL's "near '…'" text. Cancelling
  sends `KILL QUERY` over a second connection (through the same tunnel). TLS modes map to
  MySQL's: `prefer` falls back to plain only if the server offers no TLS; through a tunnel
  the certificate is checked against the real host name. MariaDB is the same driver with
  engine `mariadb` (its `JSON` is text). Read-only mode for the MCP is not implemented yet:
  it refuses these engines (ADR-008).

## Revisit if
Not expected.
