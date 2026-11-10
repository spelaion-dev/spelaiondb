# Schema explorer

Expanding a connection in the sidebar lists its schemas (databases in MySQL; system schemas
and the roles, users and logins built into each engine only with **Settings → Explorer → Show
system objects**) and, inside each,
folders by type: **Tables** (partitioned ones marked P), **Foreign tables**, **Views**,
**Materialized views**, **Functions**, **Procedures**, **Aggregate functions**,
**Sequences**, **Triggers**, **Data types** (enums, domains and other types), **Events**
(MySQL) and **Synonyms** (SQL Server); only the folders with something in them appear. After
the schemas come **Roles**, **Users**, **Logins**, **Tablespaces** and **Extensions**, as
the engine has them (MySQL's accounts only for users that may read `mysql.user`).
The plug over the explorer shows only the connected connections (those opened in the
explorer or used by a connected tab); click it again to see all of them.
The magnifying glass over the explorer (or **Ctrl+Shift+F**) opens its quick filter: only
the connections, schemas and objects whose name contains the text stay, with the folders
that hold them opened, and a connection or schema whose name matches shows everything in it.
Expanded connections stay; objects of schemas never opened are not searched. **Escape**
clears the text, and a second **Escape** closes the box.
Filters, as in DBeaver: right-click a connection (**Configure schema filter…**), a folder of
objects such as **Tables**, or a table's **Columns** (**Configure filter…**), or open the
connection's "⋯" menu (**Configure filters…** for all three). Include and exclude patterns
are separated by commas: `*` is any run of characters, `?` one character, `\` escapes the
next one, and case is ignored. A filtered folder says **filtered**; **Show everything**
removes its filter. The right-click menu of a schema, an object or a column also has **Hide**
(its name joins the excluded ones) and **Show only** (it becomes the only included one). Filters are personal: they are kept on this computer (not in the
connection's file) and change only what the explorer shows, not autocompletion (ADR-025).
They go into each engine's catalog query, so a server with thousands of schemas or tables
sends only what passes them; **Settings → Explorer → Use server-side filters** can turn that
off (every name is then read and filtered on this computer, with the same result).
The explorer keeps what it has read until you refresh it: **F5** on a schema or table (or
**Refresh schema** and **Refresh** in their right-click menus) reads it again, and **Refresh
all schemas** (in a schema's menu, the connection's "⋯" menu, or F5 on the connection) reads
everything again, autocompletion included; what is expanded stays expanded.
Names are sorted alphabetically, ignoring case; **Settings → Explorer** can keep the
database's order instead, sort with case (upper case first), or put the folders before the
schemas. Columns always keep their table's order.
**Generate DDL** in the right-click menu of a table or view opens its `CREATE` statement:
the engine's own text where it keeps one (MySQL, MariaDB, SQLite, DuckDB), composed from the
catalog for PostgreSQL and SQL Server, with constraints, indexes and comments.
Double-click a function, trigger, sequence or type (or **Generate DDL** in its right-click
menu) to open its `CREATE` statement in a new tab; nothing runs. Objects that belong to a
PostgreSQL extension are not listed: the extension stands for them.

Expanding a table shows its **Columns** (the primary key marked 🔑), and folders of its
**Constraints** (primary key, unique and check; SQLite keeps check constraints only in its
`CREATE TABLE` text), **Foreign keys** and **Indexes**; double-click an index to open its
definition. Tables and views show their estimated rows (**Count rows** in the right-click
menu gets the exact number). Double-click a table or view (or **View rows**) to open its
**table data tab**: pages of 100 to 5,000 rows with ◀ ▶, a `WHERE` and an `ORDER BY` you type,
header clicks that sort in the server, and **Count** for the exact number of matching rows.
The core writes `SELECT * … LIMIT n OFFSET m` and checks it is still one plain SELECT of that
table, so nothing typed in the filter can run another statement or load the whole table.
**Open as SQL** puts `SELECT * … LIMIT 100` in a new SQL tab instead. Right-click it to generate a `SELECT` with every column
or, for tables, a `SELECT … WHERE key =` (one row), a `SELECT … WHERE key IN` (several), an
`INSERT`, an upsert where the engine has one (`INSERT … ON CONFLICT … DO UPDATE` in PostgreSQL,
SQLite and DuckDB, `ON DUPLICATE KEY UPDATE` in MySQL and MariaDB), a `MERGE` where it has
one (PostgreSQL, SQL Server, DuckDB), an `UPDATE` or a `DELETE`, and `UPDATE … FROM` /
`DELETE … USING` with a source of `:values` (`SELECT :id AS id, …`) to replace by a real table
(SQLite has no `DELETE … USING`). **Ctrl+click** chooses several tables or views: their menu
then generates a `JOIN` (`FROM a, b WHERE …`) or an `INNER JOIN` of them on the foreign keys
between them, and **Hide** / **Show only** act on all of them. A procedure's menu has
**Generate CALL**: its input parameters as `:name` placeholders, its output ones collected
(`NULL` in PostgreSQL's CALL, `@variables` in MySQL and MariaDB, `DECLARE … OUTPUT` in SQL
Server) and shown after the call. The key is the primary key; without one, a unique key
(one whose columns cannot be NULL first); without either, every column, and the SQL says so. The core writes
them with the engine's quoting and `:name` placeholders for the values; nothing runs until
you replace them and run it.

## Searching names and data

The magnifying glass in the activity bar (or **Ctrl+H**) opens **Search** (ADR-043).

- **Names** finds tables, views, routines and other schema objects, and columns, whose name
  contains the text (case ignored; `orders.cust` finds the column by table and name), in every
  schema the explorer has read, on every connection at once. It answers from what the explorer
  already read and never asks a database while you type; the first search with **Include
  columns** reads each loaded schema's columns once, with one catalog query, and keeps them
  until the explorer reads that schema again. Click a table, view or column to open its table
  data tab, a routine to open its definition; **Enter** opens the first match.
- **Data** looks for a value in the tables you choose on one connection (**Select shown**
  takes every table the filter shows). Text and UUID columns match when they contain the
  value; under **Options**, **Match case**, **Numeric columns** (equal to the value, when it is
  a number), **Large text and binary columns** (JSON, XML, `TEXT`…, and the value's bytes in
  binary ones) and the rows each table returns at most (100 by default, up to 1,000). Dates,
  booleans and other types are not searched. The core writes one `SELECT` per table with the
  value bound as a parameter, never written into the SQL, and runs each one as a read in a
  session of its own, so it also works on read-only connections. The panel says which table it
  is on and how many hits it has found; **Cancel search** stops the running statement and the
  tables after it, and a search stops at 1,000 hits. Click a hit to open its table data tab
  filtered to that row (by its primary key or, without one, by the matching value). SQLite
  files on a server have no data search.

## Editing data

In a table data tab of a table with a primary key, double-click a cell (or **F2**) to change
it; the right-click menu also has **Set NULL**, **Delete row** (or the **Del** key) and
**Revert changes**, and **Add row** opens a form (empty fields take the column default).
Changes stay pending, drawn in the grid (edited cells highlighted, deleted rows struck
through), until **Apply** or **Discard**; paging and filtering wait meanwhile. The core
writes the `UPDATE`/`DELETE` statements by primary key (read from the database, with the key
values of the loaded rows) and the `INSERT`s, checks each typed value against its column type,
and applies everything in one transaction: if a statement does not affect exactly one row
(the row changed or vanished meanwhile), nothing is applied. Production connections show the
statements for confirmation first, and read-only connections refuse. Tables without a primary
key are read-only (ADR-014).
