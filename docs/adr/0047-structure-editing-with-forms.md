# ADR-047: Structure editing with forms: one script from the core, applied only as it was shown

- **Status:** Accepted
- **Date:** 2026-11-10
- **Builds on:** ADR-026 (the explorer's object actions), ADR-008 (read-only mode), ADR-029
  (connection types and their safeguards), ADR-031 (transactions)

## Context
ADR-026 made structure changes safe by opening a reviewed SQL template, or by showing the SQL
of a rename or a drop before running it. That is slow for the most common changes: a column's
type, its nullability, its default or its comment, often several columns at a time. Each engine
writes these differently (`ALTER COLUMN … TYPE … USING`, MySQL's `MODIFY COLUMN` that restates
the whole column, SQL Server's named default constraints and extended properties, DuckDB's one
action per statement), and SQLite cannot do most of them at all: it must create the table again.

## Decision
1. **A form sends what the columns should become, never SQL.** The core reads each column for
   the form (`table_structure`), every field as SQL the engine writes: the type as declared and
   the default as an expression (`'text'` with its quotes; on MySQL and MariaDB taken from
   `SHOW CREATE TABLE`, not from `information_schema`, which drops the quotes). The form sends
   the new state of the columns it shows and the constraints to drop (`TableAlteration`).
2. **The core writes one script from the catalog read again each time**
   (`spelaion-core::objects::alter`), changing only what differs from the catalog: types compared
   as SQL (case and blanks do not count), a default or a comment left empty is none. A type or a
   default typed in the form must be one piece of SQL (closed quotes and parentheses, no `;`).
   - PostgreSQL: one `ALTER TABLE` with every action (`TYPE … USING col::type`, `SET/DROP NOT
     NULL`, `SET/DROP DEFAULT`), then `COMMENT ON COLUMN`.
   - MySQL and MariaDB: one `ALTER TABLE … MODIFY COLUMN …` per table, each column definition
     taken from `SHOW CREATE TABLE` with only the changed parts rewritten, so its collation,
     `AUTO_INCREMENT`, `ON UPDATE`, comment and other attributes are written again as they were
     (a type that holds no text loses its character set and collation).
   - SQL Server: `ALTER COLUMN` restates the nullability and the collation, which it would
     otherwise reset; a default is a named constraint, dropped before a type change and added
     again with its name; comments are `MS_Description` extended properties.
   - DuckDB: one statement per action, and `COMMENT ON COLUMN`; it cannot drop constraints.
   - Dropping a constraint uses ADR-026's statements, except on SQLite.
3. **SQLite rebuilds the table**, following SQLite's own procedure: foreign keys off if they were
   on (outside the transaction, where the pragma works), then in one transaction the views and
   other tables' triggers that name the table (and those that name them) are dropped, the new
   table is created from the old one's stored text with only the change made (`ddl.rs` edits
   that text, so collations, checks, generated columns, `AUTOINCREMENT` and `WITHOUT ROWID` stay),
   the rows are copied, the old table dropped and the new one renamed, `AUTOINCREMENT`'s counter
   set back, and the indexes, triggers, views and triggers are created again from the text SQLite
   kept. With foreign keys on, `foreign_key_check` on the table and the tables that point to it
   must find nothing, or everything rolls back. Foreign keys are switched on again even when it
   fails. A rebuild drops unique constraints and foreign keys too (SQLite keeps no names for
   them: they are found by their columns and referenced table).
4. **The script is shown in full, as it follows the form**, with the statements that run before
   and after the transaction. It can be opened in the editor instead of applied.
5. **Apply runs exactly what was shown.** The UI sends the form again with the script it showed;
   the core writes the script again from the catalog and refuses it if it differs (the table
   changed in between). It refuses a read-only connection and a session with a transaction open
   (ADR-031: never nested in, nor committing, the user's). The statements `guard.rs` would ask
   about (the rebuild's `DROP TABLE`, any change where the connection's type confirms data
   changes, every run where it confirms execution) run only once the user confirmed them in the
   usual risk dialog. Then the statements run in order on the tab's session: in one transaction
   where the engine's DDL is transactional (all but MySQL and MariaDB, where each statement
   commits on its own and the dialog says so), rolled back at the first failure, which names the
   statement and what the engine answered.
6. **New tables come from a form too** (`create.rs`): columns (type, nullability, default,
   comment), a primary key, foreign keys and indexes become one script (`CREATE TABLE` with its
   constraints, then `CREATE INDEX` and comments), validated like the column form and applied
   the same way: only as shown, never read-only, risky statements confirmed. A constraint or
   index left without a name is named after the table and its columns. This replaces ADR-026's
   "New table" template for tables; the script can still be opened in the editor.
7. **A routine's or view's source is edited in the editor** (`source.rs`): "Edit source" opens
   its definition written as the statement that replaces it, where the editor's barriers apply
   (read-only mode, the guard, the connection's type) and nothing runs until the user does:
   `CREATE OR REPLACE` on PostgreSQL, DuckDB and MariaDB (a materialized view cannot be
   replaced), `CREATE OR ALTER` on SQL Server, `CREATE OR REPLACE VIEW` on MySQL but a function
   or procedure dropped (`DROP … IF EXISTS`) and created again between `DELIMITER` lines, and
   `DROP VIEW IF EXISTS` before SQLite's view.
8. **Running a routine writes its call with a parameter form:** each driver's `procedure_call`
   now writes a function's call too (`SELECT * FROM f(:a)` on PostgreSQL, `SELECT f(:a)` on
   MySQL and MariaDB, `SELECT dbo.f(:a) AS result` or `SELECT * FROM` a table-valued one on SQL
   Server, a DuckDB macro or table macro), never a procedure as a function or the other way
   round. "Run…" opens the call in a new tab and runs it once the tab is connected, like Run
   script: the editor's parameter form asks the values and the core writes them into the call as
   literals, then the usual barriers apply. The request to run is cleared before it runs and
   never saved with the tab, so nothing runs again after a restart or a reconnect.
9. **Where the forms are:** "Edit columns…" in a table's Columns view (on the chosen columns, or
   all of them) and "Edit column…" in a column's explorer menu (on the column, or the columns
   chosen with Ctrl+click); a SQLite unique constraint's or foreign key's "Drop…" opens the
   rebuild's script; "New table…" in a schema's and its Tables folder's menu opens the create
   table form; "Edit source" is in a view's, function's and procedure's explorer menu, "Run…"
   and "Generate CALL" in a function's and procedure's.

## Consequences
- Each engine's statements are tested by running them on that engine
  (`spelaion-core/tests/object_actions.rs`), including a SQLite rebuild that keeps its rows,
  indexes, triggers, views, keys and `AUTOINCREMENT` counter, and one that breaks a key and
  changes nothing.
- A structure change made by a form can never run SQL the user has not seen, nor run against a
  table that is no longer the one the SQL was written for.
- MySQL and MariaDB cannot apply several statements atomically; a form there writes one
  `ALTER TABLE` for the columns, and constraint drops before it are their own statements.
- `tests/object_actions.rs` also edits and runs every engine's views, functions and procedures
  from their editable source, and calls a function with its parameters bound.
