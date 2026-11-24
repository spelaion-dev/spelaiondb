# ADR-069: Copying tables between connections: an import whose source is a read

- **Status:** Accepted (being implemented)
- **Date:** 2026-11-24
- **Builds on:** ADR-020 (read barriers), ADR-029 (connection types), ADR-031 (transactions),
  ADR-034 (streaming exports), ADR-038 (imports), ADR-060 (jobs), ADR-066 (ClickHouse imports)

## Context
Copying a table from production to a local database, or from MySQL to PostgreSQL, takes a dump,
hand edits and an import today (SDB-35). The pieces it needs already exist and have their rules:
the streaming export (ADR-034) reads a table a batch at a time inside the source engine's read
barrier, and the import (ADR-038) creates a table, loads text rows into it in transactions
through `insert_rows` (values never SQL text), and goes through the target's barriers. What is
missing is reading types across engines: a column's type on one engine is not a type on
another, and some have no exact counterpart.

## Decision
1. **A copy is an import whose rows come from a read of another connection.** One or more
   tables or views, chosen in the explorer of a connection A, go into a schema of a connection B,
   on any engine. For each one the user chooses:
   - structure and data: a new table, then its rows;
   - structure only: the new table;
   - data only: into a table that is there, columns matched by name.

   If the name is taken, the copy fails, replaces the table (a `DROP`, which asks first) or
   appends to it, as an import does.
2. **What it copies:** columns, their nullability and the primary key. Indexes, foreign keys,
   defaults, checks, triggers and comments are not copied, and the preview says so: their SQL is
   the source engine's, and rewriting it is a schema compare's work, not a copy's.
3. **The source is the streaming export's read** (ADR-034):
   - a new session from A, which never touches a tab's;
   - the core's own `SELECT` of the table;
   - A's engine's read barrier, always rolled back;
   - one batch at a time, never the table in memory.

   It is refused where A's server does not keep its read barrier (Babelfish, RisingWave, the
   servers ADR-020 lists), as streaming exports are. A read-only connection may be a source.

   The user learns of that refusal before choosing anything. The plan reads the source first:
   its catalog, then a read of no rows inside the barrier. A source whose server does not keep
   the barrier fails right there, with the server family's own reason (Babelfish's, for
   example). So the dialog shows that reason in place of the column list, as soon as the source
   table is chosen.
4. **The target is the import's load** (ADR-038), fed by those batches instead of a file. So
   everything the import guarantees holds unchanged:
   - the DDL is written by the core and shown first;
   - the guard runs on every statement:
     - a replace's `DROP TABLE` asks first on every connection, whatever its type (the guard
       always asks for a `DROP`, Development included);
     - on ClickHouse it is the `DROP` of what `EXCHANGE TABLES` swapped out, and it asks the
       same;
     - a connection type that confirms data changes (Production) asks for the rows' insert, and
       one that confirms execution asks for everything;
     - the core refuses a run that skipped a confirmation it needed;
   - a read-only target is refused, and so is a server whose transactions do not hold (SDB-354);
   - rows load in transactions of `commit_every` rows, or on ClickHouse into a table of the
     import's own, put in place whole (ADR-066);
   - only whole batches stay.
5. **One job, one cancel** (ADR-060). A cancel stops the source's running statement (the
   engine's cancel) and the load. As SDB-448 ruled, it never interrupts the target's own
   `BEGIN`, `COMMIT`, `ROLLBACK`, or the statements that put ClickHouse's table in place: there
   it only sets the flag that is checked right after. Progress counts rows read and committed.
6. **Types are mapped by the core**, in two steps, and every loss is named:
   - **A source column becomes a portable kind.** Its logical type (what the plugin already
     reads it as) and its native type (sizes, precision, length, unsigned) together give it:
     boolean; integer of 1, 2, 4 or 8 bytes, signed or not; decimal with its precision and scale;
     float of 4 or 8 bytes; text with its length; binary; date; time; timestamp; timestamp with
     zone; UUID; JSON; or other (intervals, arrays, an engine's own types).
   - **That kind becomes a type of the target engine**, with the losses it has there. For
     example:
     - a timestamp with zone goes to MySQL, MariaDB or ClickHouse as its UTC time;
     - a UUID goes to MySQL or SQLite as text;
     - JSON goes to SQL Server, SQLite or ClickHouse as text;
     - a decimal wider than the target allows (MySQL 65 digits, SQL Server and DuckDB 38)
       becomes the widest the target has, and a value that does not fit fails the copy;
     - an unsigned integer goes where there are none into a wider type;
     - "other" goes as text.

   A loss is never silent: the preview lists it on its column. The user may change a target type
   (checked as an import's typed type is).
7. **Values travel as the core's canonical text.** That is `Value`'s own text: ISO dates,
   timestamps with their microseconds, decimals as written, booleans as `true`/`false` (turned
   into `1`/`0` where the target column is a `bit` or a `tinyint(1)`, as imports do), and floats
   at their shortest exact form. The import's normalizer adds what each target needs, such as a
   timestamp with zone written as UTC for MySQL and ClickHouse. A value the target refuses
   (`NaN` in MySQL, a year before 1 on most engines) fails the batch it is in, as a bad value
   does in an import, and says which rows.
8. **Binary columns** travel as hex text (`insert_rows` carries text: plugin protocol 1.0).
   PostgreSQL and DuckDB read that text as bytes. MySQL, MariaDB, SQLite and ClickHouse would
   store the hex text itself, and SQL Server refuses it. So into those five targets a binary
   column cannot be copied yet: the preview says so, and the copy is refused unless the user
   leaves the column out. Binary columns in `insert_rows` (protocol 1.2, every SQL plugin) are
   SDB-464, in the same release (v0.16): the copy is not done until binary copies between every
   pair too.
9. **Tests:** the done-when sets the bar. A table with every common type, created on each engine
   that can be a target (PostgreSQL, MySQL, MariaDB, SQL Server, SQLite, DuckDB, ClickHouse),
   is copied to each of them (49 pairs), and the data read back is compared value by value,
   with each pair's expected losses asserted.

## Alternatives
- **Dump and restore (`pg_dump`, `mysqldump`, their SQL):** exact within one engine, but useless
  across engines, and it needs each engine's tools installed and on the PATH.
- **The source's DDL, rewritten for the target:** keeps more (defaults, indexes), but every
  engine's DDL would need a translator, and one mistake is a broken or a different table. The
  mapped columns and the primary key are what a copy can promise.
- **Arrow batches straight to the target:** no text in between, but `insert_rows` takes text
  (protocol 1.0), and the server's own conversion from text is what imports are tested on.
- **Binary in this first version, through a new protocol first:** every SQL plugin would have to
  be released before the copy lands. Binary into PostgreSQL and DuckDB works now, and for the
  rest the copy says so.

## What would change this
- If users need indexes, defaults or foreign keys copied, they come from the schema compare's
  DDL (v0.15's other headline), not from a copy's own translator.
- If text conversions turn out too slow for big copies, a typed (Arrow) `insert_rows`.
