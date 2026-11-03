# ADR-020: The MCP serves MySQL, MariaDB, SQL Server and DuckDB

- **Status:** Accepted
- **Date:** 2026-11-03
- **Supersedes:** the "not in `spelunk mcp` yet" decisions of ADR-016 (DuckDB) and ADR-017
  (SQL Server), and the MySQL limit noted in ADR-008

## Context
`spelunk mcp` only served PostgreSQL and SQLite: those were the engines with a read-only
mode of their own (ADR-008). Users want their agents on every engine the app supports. Each
engine needs barriers it actually enforces, not only our classifier: a user function that
writes looks like a read, and DuckDB reads local files and the network from a plain
`SELECT`.

## Decision
Every engine keeps ADR-008's layers (classifier, the engine's own barrier, limits) and
ADR-019's writes. What changes per engine is the second layer:

| Engine | Reads (always rolled back) | Writes |
|---|---|---|
| PostgreSQL | `BEGIN READ ONLY`, `statement_timeout` | `BEGIN` … `COMMIT` |
| SQLite | connection opened read-only, `PRAGMA query_only` | `BEGIN` … `COMMIT` |
| MySQL, MariaDB | `START TRANSACTION READ ONLY` | `START TRANSACTION` … `COMMIT` |
| SQL Server | `BEGIN TRANSACTION`, always `ROLLBACK` (T-SQL has no read-only transaction; its functions cannot change data); `EXPLAIN` runs under `SET SHOWPLAN_XML ON`, where nothing runs | `BEGIN TRANSACTION` … `COMMIT` |
| DuckDB | `enable_external_access = false` (no files, network nor extensions; DuckDB will not turn it back on while the database is open) and `BEGIN TRANSACTION READ ONLY`; the file opened read-only unless the connection lets agents write | external access off, `BEGIN TRANSACTION` … `COMMIT` |

- **Time limit:** PostgreSQL's server-side `statement_timeout`; elsewhere a client timer that
  cancels (`KILL QUERY`, a TDS Attention, DuckDB's interrupt) and rolls back.
- **Row limit:** reading stops at `max_rows`; the rest of the result is not kept.
- **What reaches outside the database is refused by name, for reads and writes**
  (`classify.rs`): server files (`pg_read_file`, `LOAD_FILE`), other servers (`dblink`,
  `OPENROWSET`, `OPENQUERY`, SQL Server's four-part names), environment variables and files
  in DuckDB (`getenv`, `read_csv`, …), and locks or settings that outlive the statement
  (`pg_advisory_lock`, `set_config`, `GET_LOCK`). For DuckDB the engine setting is the real
  barrier and the names only give a clearer message.
- **Writes keep to an allow list** (`write.rs`), on every engine: `INSERT`, `UPDATE`,
  `DELETE`, `MERGE`, `COMMENT`, and `CREATE`/`ALTER` of tables, views, indexes, schemas and
  sequences. Procedures (`CALL`, `EXEC`), permissions (`GRANT`, users and roles), settings
  (`SET`), secrets, routines and triggers are refused, as are ADR-019's destructive
  statements, transaction control and other files.
- **DuckDB opens a file in one mode per process** (ADR-016): where agents may write, the
  MCP's read session shares the writable file and relies on the read-only transaction.

## Alternatives
- **Only the classifier for the new engines:** it cannot see what a function does. The
  engine's own barrier stays.
- **A deny list for writes (ADR-019 as first written):** it lets through whatever nobody
  thought of (`GRANT`, `EXEC`, `SET`). An allow list refuses the unknown, like the
  classifier for reads.
- **DuckDB's `allowed_directories`:** agents could read files next to the database, but
  which files is a decision the user never made. Revisit if someone needs it.

## Consequences
A read-only database user is still the only real guarantee, and the README says so. In
SQL Server and MySQL, DDL is outside the read barriers by design: the classifier refuses it
before it is sent, and MySQL's implicit commit means a write's DDL is committed on its own.

## Revisit if
An engine gains a real read-only transaction (SQL Server), or agents need statements the
allow list refuses (then a narrower exception, not a wider door).
