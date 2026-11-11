# ADR-052: Column masking also follows the table column a result column reads

- **Status:** Accepted
- **Date:** 2026-11-11
- **Amends:** ADR-051 (column masking): its rules match by result column name only, and it
  left out `table.column` rules and masking aliases
- **Builds on:** ADR-005 (driver contract, Arrow metadata per column), ADR-041/042 (the
  contract plugins implement)

## Context
ADR-051 matches masking rules against the name a result gives a column. An alias gets past
it: `SELECT email AS e FROM users` returns `e`, and `*email*` does not match `e`. A person can
do that on purpose, and ADR-051 says masking is not access control. For agents over MCP, it
happens without anyone meaning it: models alias columns all the time (`SELECT u.email AS
contact`). An alias on a masked column is the most common way a masked value would reach an
agent.

Most engines can say which table column a result column reads, when it reads one directly:
- PostgreSQL's row description gives each column its table's OID and column number.
- MySQL's and MariaDB's column definitions carry the original table and column names.
- SQLite reports a column's origin when it is compiled with column metadata.

## Decision
1. **Drivers report each column's origin where the engine says it**, as owned data in the
   column's Arrow metadata: `spelaion.origin_table` and `spelaion.origin_column`
   (`ColumnMeta::origin`, `ColumnOrigin`). It is part of the driver contract, so a plugin
   (ADR-041) reports it the same way. Only the core's masking reads it; the grid never does.
   | Engine | Origin | How |
   |---|---|---|
   | PostgreSQL | yes | the row description's table OID and column number, resolved in `pg_attribute` and `pg_class` before the statement runs. That is one extra catalog read per statement, only when the session masks something (`ExecOptions::origins`). It is never cached, so a renamed column is seen at once. If the catalog cannot be read, no column has an origin. |
   | MySQL, MariaDB | yes | the column definition's `org_table` and `org_name` |
   | SQLite (a local file) | yes | `sqlite3_column_table_name` and `sqlite3_column_origin_name` (rusqlite's `column_metadata`) |
   | SQLite on a server (ADR-018) | no | its `sqlite3` program prints values, not metadata |
   | SQL Server | no | TDS gives a table name only for the legacy `text`, `ntext` and `image` types, and only in browse mode otherwise |
   | DuckDB | no | its result API has no origin |
   An expression (`lower(email)`, `email || ''`, a function, a constant) has no origin on any
   engine. A column read through a view has the view's own column as its origin on PostgreSQL
   and MySQL, and the base table's column on SQLite.
2. **Rules match the origin too, always erring towards masking.**
   - A column pattern (`*email*`, `card_number`) masks a column when it matches the result
     name **or** the origin's column. `SELECT email AS e` stays masked wherever the engine
     reports the origin.
   - **`table.column` patterns** are now allowed, split at the last dot (`users.email`,
     `*.ssn`). With a known origin, both parts must match. With an unknown one, the column part
     alone is matched against the result name. So on SQL Server and DuckDB, `users.email` masks
     every column named `email`, rather than masking none.
   - A **`never`** pattern exempts a column only by its origin when the origin is known.
     Renaming a masked column to an exempted name (`SELECT email AS email_verified`) does not
     unmask it. A `never` written as `table.column` exempts nothing when the origin is unknown.
   - Required patterns (the connection type's, the user's on a repository connection) follow
     the same matching, and still no `never` exempts them (ADR-051).
3. **Where the core reads a table column by its own name**, the origin is that table column:
   column statistics, the data search and the foreign key picker. A table data tab's page is a
   query like any other, and gets its origins from the driver.
4. **Everything else in ADR-051 stands.** Values are replaced where results enter the core, and
   only one cell is revealed at a time. Masking is still not access control: an expression
   gets past it on every engine, and an alias does on the engines without origins (the table
   above). Database permissions are the barrier for that.

## Alternatives
- **Parsing the SQL to trace each output column to its source.** It would also work on SQL
  Server and DuckDB. But it needs the catalog to expand `*` and resolve names, every dialect's
  scoping rules, views and CTEs. A parser that is wrong unmasks silently, while the engine's own
  answer is exact.
- **SQL Server's `sp_describe_first_result_set` with browse information**, run before each
  statement. It gives source tables and columns, but costs a round trip per statement, covers
  only the first result set, and fails on temporary tables and dynamic SQL. It is worth
  revisiting if teams need masking on SQL Server.
- **Refusing agents' queries that alias a masked column.** It needs the same tracing as parsing,
  and an agent would just be refused instead of getting the mask.
- **Caching PostgreSQL's OID-to-name lookups per connection.** It saves a round trip, but a
  renamed column would keep its old name until the cache is refreshed, and a stale name can
  unmask.

## Revisit if
- DuckDB's API starts reporting column origins, or SQL Server is needed (see the alternatives).
- A cheap per-statement cost on PostgreSQL turns out to matter over slow tunnels. The lookup
  could then be pipelined with the statement.
