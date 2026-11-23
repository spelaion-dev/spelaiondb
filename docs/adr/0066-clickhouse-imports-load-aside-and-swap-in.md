# ADR-066: Importing into ClickHouse: a new table loaded aside and put in place whole

- **Status:** Accepted (implemented)
- **Date:** 2026-11-23
- **Builds on:** ADR-038 (importing data), ADR-041 (engines as plugins)
- **Supersedes:** ADR-038's point 13, for ClickHouse only

## Context
ADR-038 loads an import's rows in transactions and rolls back the one that fails, so a failed
or cancelled import leaves only whole batches and says how many rows were committed. Its point
13 (SDB-354) refuses an import wherever transactions do not hold. ClickHouse has none: its
experimental transactions need ClickHouse Keeper and are off by default. Its `INSERT` is atomic
only within one block of one partition, so a large insert that fails can leave some of its
blocks written. SDB-324 cannot turn imports on with the load as ADR-038 describes it.

What ClickHouse does make atomic is renaming tables. In an `Atomic` database (the default since
20.10), `RENAME TABLE a TO b` and `EXCHANGE TABLES a AND b` happen whole or not at all.

## Decision
1. **On ClickHouse an import goes into a new table, created whole or not at all.** The rows
   load into a table of our own in the same database, created from the plan's
   `CREATE TABLE`. They go through `insert_rows`'s `INSERT … FORMAT TabSeparated`, with the rows
   in the request's body, so values are never SQL text (ADR-038's point 5).
2. **The table of our own is named `__spelaion_import_` followed by a random v4 UUID** (32 hex
   digits: 122 random bits). Its `CREATE TABLE` has no `IF NOT EXISTS`, so a name that is
   already taken, however unlikely, fails the import before any row is sent. The import only
   ever drops a table its own `CREATE TABLE` made: it never touches a table it did not create.
   The prefix tells these tables apart in the explorer and in `system.tables`.
3. **When every row is in, one statement puts the table in place.** For a new table it is
   `RENAME TABLE … TO <table>`, which fails if a table of that name appeared meanwhile. When the
   import replaces a table, it is `EXCHANGE TABLES … AND <table>`, then `DROP TABLE` of what was
   swapped out. That `DROP` is shown and asks first, like ADR-038's replace. Both are a new
   `finish` list of the plan, shown after the rows' statement and read by the guard like the
   others.
4. **A failure or a cancel drops the table of our own** and leaves the target as it was. The
   result says no row was committed. `commit_every` does not apply: the whole import is one
   unit, as with `commit_every = 0` elsewhere.
5. **A database that is not `Atomic` is refused** before anything runs (`Ordinary`,
   `Replicated` and the others do not promise the swap). So is **appending to an existing
   ClickHouse table**, since no way of adding rows to it can be undone. The user imports into a
   new table and runs `INSERT … SELECT` themselves, seeing what it does.
6. **Tables left behind.** If the app or the connection dies mid-import, the table of our own
   stays, holding some rows. It is never dropped automatically: another window or another
   machine may be loading into it right now, and its name says nothing about that. Instead,
   every ClickHouse import's plan lists the tables of that database whose names start with the
   prefix (`leftovers`). The dialog shows them, saying they were left by imports that did not
   finish and can be dropped once no import is running. The user drops them from the explorer,
   with its usual confirmation.
7. **A new table's columns are `Nullable(…)` of ClickHouse's types** (`Int64`,
   `Decimal(p, s)`, `Float64`, `Bool`, `Date32`, `DateTime64(6)`, `String`), so a missing value
   stays NULL. Its engine is `MergeTree ORDER BY tuple()`, with no sort key the user did not
   choose. A timestamp with a zone is stored as its UTC time, as on MySQL. A type the user
   types may wrap others (`Nullable(Decimal(20, 2))`), still with only words, digits, commas and
   balanced parentheses.
8. Other engines without transactions keep ADR-038's point 13: each would need its own way to
   make the load all or nothing.

## Alternatives
- **Accept a partial load and report it:** simplest, but after a failed insert the summary
  could not say which rows are in. That is why SDB-354 refused it.
- **A table of our own, then `INSERT … SELECT` into an existing table:** the last insert is no
  more atomic than the first, so appending would look safe and not be.
- **ClickHouse's experimental transactions:** off by default and need Keeper, so most servers
  users connect to do not have them.
- **Dropping leftover tables automatically, after an age:** a large import on a slow link can
  outlive any age we choose, and dropping it would break an import that is still running.
- **A name derived from the target table:** readable, but two imports into the same name
  would collide, and the name could grow past what a user's tools expect.

## What would change this
- If ClickHouse's transactions become generally available, the ordinary load with them.
- If users need to append, `ATTACH PARTITION … FROM` the table of our own, one partition at a
  time, with the result saying which partitions went in.
