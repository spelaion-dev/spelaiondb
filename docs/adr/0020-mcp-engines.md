# ADR-020: The MCP serves MySQL, MariaDB, SQL Server and DuckDB

- **Status:** Accepted (amended by ADR-072: for agents, the names refused here stay and a
  function must also be on a list to be called)
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
  sequences. Since 0.5 (issue #7), `CREATE OR REPLACE` of a table, schema or materialized view
  is refused, and `ALTER TABLE` may only add columns or constraints and rename: an agent cannot
  drop a column, change its type or drop a partition. MySQL and MariaDB executable comments
  (`/*! … */`, `/*M! … */`) are refused before anything is parsed: the server runs what the
  parser reads as a comment. Procedures (`CALL`, `EXEC`), permissions (`GRANT`, users and roles), settings
  (`SET`), secrets, routines and triggers are refused, as are ADR-019's destructive
  statements, transaction control and other files.
- **DuckDB opens a file in one mode per process** (ADR-016): where agents may write, the
  MCP's read session shares the writable file and relies on the read-only transaction.

### Servers that speak PostgreSQL's protocol (2026-11-14, SDB-169)
A PostgreSQL connection may reach a server that only speaks PostgreSQL's protocol, through the
PostgreSQL plugin. Which one is read from the server (`select version()`) when a session opens,
never from the connection's file or the plugin (`spelaion-core`'s `server_family.rs`), and
the PostgreSQL row of the table above applies only where the server enforces it, checked against
each server's image (`testing/docker/pg-family`):

| Server | `BEGIN READ ONLY` | Session's read-only mode | What it gets |
|---|---|---|---|
| PostgreSQL (and TimescaleDB, an extension), CockroachDB 24.3, YugabyteDB 2026.1 | enforced | enforced | everything |
| RisingWave 3.1 | enforced | accepted with a notice, ignored | no read-only connections, no agents (2026-11-23, below); streaming reads |
| Materialize 26.50 | refuses `INSERT`, lets `CREATE TABLE` through | refused | no agents, read-only connections or streaming reads |
| CrateDB 6.5 | accepted, ignored (no transactions) | accepted, ignored | no agents, read-only connections or streaming reads |
| A fork that names itself and was not checked (Redshift, Greenplum…), or a server whose version does not start with `PostgreSQL` | not assumed | not assumed | no agents, read-only connections or streaming reads |

The refusal comes when the session opens (a read-only connection, agents' sessions) or before
the read is sent (streaming exports, column statistics, the data search), with the reason.

**2026-11-23, RisingWave gets no agents (SDB-302).** Checked against RisingWave 3.1.1, whose
read-only transaction refuses every write and DDL sent inside it, and changes nothing when
`SET TRANSACTION READ WRITE` or `BEGIN READ WRITE` is sent inside it:
- **It does not stop the table functions that reach outside.** `postgres_query` and
  `mysql_query` run their text on another server (checked: one reached the test MySQL from
  inside `BEGIN READ ONLY`), `file_scan` reads a bucket (it reached S3), and
  `openai_embedding` sends its text to a remote service. The classifier refuses the four on
  every PostgreSQL connection, before anything is sent.
- **That list cannot be the barrier.** RisingWave lists none of its built-in functions
  (`pg_proc` is empty, `information_schema.routines` is not implemented, `SHOW FUNCTIONS` and
  `rw_catalog.rw_functions` list only user functions), so it cannot be checked complete
  against the server. Any view may also call one, and the statement then only names the view:
  a view over `postgres_query` passes the classifier and runs its query on the other server
  inside the read-only transaction (`tests/postgres_family.rs` keeps that evidence).
- **So agents' reads stay refused** when their session opens, as before (their session is a
  read-only connection's, whose mode RisingWave ignores).
- **Agents' writes are refused** on any server whose transactions do not hold
  (`Session::atomic_writes_refusal`). They run in a transaction of their own (ADR-019), and
  RisingWave's writes are not transactional. Before this, an agent's write session opened on
  RisingWave.

Offering agents on RisingWave would need a barrier that a view cannot get around, for example
a read-only database user that cannot reach other servers, decided in an ADR of its own.

### Servers that speak MySQL's protocol (2026-11-15, SDB-170)
The same for a MySQL connection, read from `select version(), @@version_comment`. A MySQL
server's text names its distributor rather than MySQL (managed services say "Source
distribution", Percona its own name), so what names none of the forks below is taken for MySQL,
and after a read-only connection's session is made read-only the core reads
`@@session.transaction_read_only` back (`current_setting('transaction_read_only')` on
PostgreSQL): a server that accepted the statement and is still writable is refused there,
whatever it calls itself.

| Server | `START TRANSACTION READ ONLY` | Session's read-only mode | What it gets |
|---|---|---|---|
| MySQL, MariaDB, OceanBase 4.3 (MySQL mode) | every write but DDL refused (DDL commits first; the classifier refuses it, as above) | enforced, DDL too | everything |
| TiDB 8.5 | refused as a no-op a setting can switch on | the same | no agents, read-only connections or streaming reads |
| SingleStore 10.9, Apache Doris 3.0 | a syntax error | accepted, session stays writable | no agents, read-only connections or streaming reads |
| StarRocks 3.4 (says it is MySQL; recognised by its `current_version()`, SDB-310) | a syntax error | accepted, session stays writable | no agents, read-only connections or streaming reads |

### Babelfish through SQL Server's protocol (2026-11-23, SDB-313)
Babelfish for PostgreSQL speaks TDS in front of PostgreSQL. A SQL Server connection to it is
told apart from `@@VERSION` ("Babelfish for PostgreSQL…") when the session opens; a server that
does not say so keeps SQL Server's barrier, unchanged. Checked against Babelfish 5.6.0
(PostgreSQL 17.10), built in `testing/docker/babelfish`:
- **Its transactions hold.** A write rolled back is gone, so table edits, imports and agents'
  writes, which rely on a transaction of their own, work as on SQL Server.
- **SQL Server's barrier does not hold.** The barrier rests on T-SQL functions that cannot change
  anything. On Babelfish, T-SQL reaches PostgreSQL's own functions:
  - `pg_backend_pid()` and `set_config(...)` answer;
  - a `SELECT` of `pg_try_advisory_lock` inside a transaction that is then rolled back leaves the
    lock held (`tests/babelfish.rs` keeps that evidence);
  - `pg_advisory_lock` takes its lock even though the statement then fails on its `void` result.
  The classifier could name them, as it does on PostgreSQL connections, but any view may call
  one, and the statement then only names the view.
- **So Babelfish gets no agents' reads and no read-only connections**, refused when the session
  opens, with the reason. Nor does it get the reads that rely on the same barrier (streaming
  exports, column statistics, the data search), refused before they are sent.
- **The SQL Server plugin follows Babelfish's differences** (its 0.1.2):
  - the transaction state comes from `XACT_STATE()`, since Babelfish sends no transaction
    descriptor;
  - plans are not declared, since Babelfish has none;
  - a cancel, which Babelfish would answer only once the statement ends on its own, stops the
    statement from a second connection: with `pg_cancel_backend` where the login may, otherwise
    with `KILL`, which ends the session and says so.

Offering agents on Babelfish would need a barrier a view cannot get around, such as a
PostgreSQL role for agents that cannot reach those functions, decided in an ADR of its own.

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
