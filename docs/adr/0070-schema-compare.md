# ADR-070: Schema compare: two catalogs read by the core, a report per object, a script opened in the editor

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Builds on:** ADR-024 (the explorer's catalog: `list_objects`, `describe_table`,
  `list_schema_objects`, `object_definition`, `relation_definition`), ADR-026 (SQL from the
  core, run only after it is shown, in the editor), ADR-047 (structure editing: columns read as
  SQL the engine writes, one script per change, SQLite rebuilds), ADR-060 (a job's lifecycle,
  cancel and ending)
- **Decides:** SDB-34

## Context
"What changed between staging and production?" is asked often and answered badly: by eye, or
with a paid tool. The pieces are already in the core. The explorer reads every engine's
catalog through the driver contract (ADR-024). The structure forms read a table's columns as
SQL the engine writes and turn a column's new state into each engine's statements, a SQLite
rebuild included (ADR-047). The explorer's actions write `CREATE INDEX`, constraints and drops
(ADR-026), and "Edit source" writes a view or a routine as the statement that replaces it.

What is missing is reading two schemas, saying what differs, and writing one script that makes
the second like the first, without ever running it from the comparison.

## Decision
1. **What is compared.** Two schemas, *source* and *target*, on the same connection or two
   connections of the same engine and dialect (MySQL is not compared with MariaDB, nor
   PostgreSQL with a server of its protocol that `server_family.rs` does not recognise as
   PostgreSQL). The engines are those whose structure forms exist (ADR-047): PostgreSQL, MySQL,
   MariaDB, SQL Server, SQLite and DuckDB. The app asks the core whether a session can compare;
   it never decides by the engine's name. On every other engine (Oracle, ClickHouse, the
   servers of PostgreSQL's and MySQL's protocols that are not those engines, and every engine
   without structure forms) "Compare schemas…" is shown unavailable with the core's reason,
   never hidden, and each is a follow-up that comes with its structure forms. Both connections
   must already be connected in the explorer: comparing never asks for a password or a trust
   decision on its own. Each side is
   read in a session of its own, opened from that connection for reads only, as exports and the
   data search are (ADR-034, ADR-043), so the explorer stays usable during a long comparison
   and a cancel stops the reads; both sessions are closed when the comparison ends.
2. **The core reads a snapshot of each side** (`spelaion-core::schema_compare`), only through
   what the explorer and the forms already read: the schema's tables and views
   (`list_objects`), each table's columns as the forms read them (type, nullability, default as
   SQL, comment, generated or not: ADR-047's `table_structure`), its primary key, unique and
   check constraints, foreign keys and indexes (`describe_table`), its comment, each view's
   definition (`relation_definition`), and its functions and procedures with their definitions
   (`list_schema_objects`, `object_definition`). Nothing is read that the user's connection
   could not read in the explorer. The reads are a job (ADR-060): one cancel, progress per
   object, one ending; a side that fails ends the comparison with its reason.
3. **Objects are matched by kind and name**, as the catalog writes the name. A function or
   procedure is matched by its name and its `detail` (PostgreSQL's and DuckDB's arguments), so
   overloads are compared one by one. A constraint or index is matched by its name; where an
   engine keeps no name (SQLite's unique constraints and foreign keys), by its columns and
   referenced table, as ADR-047's rebuild finds them.
4. **What counts as a difference** is decided in one place, per property, the way the forms
   already decide it: types compared as SQL (case and blanks do not count, `same_type`), a
   default or comment left empty is none, a primary key, unique or check constraint by its
   kind, columns and definition as the catalog writes it, an index by its definition (the
   `CREATE INDEX` the driver writes, its own table's name left out), a foreign key by its
   columns, referenced table (in the target schema when it pointed into the source schema) and
   referenced columns, a view's or routine's source by its text with line endings, trailing
   blanks and runs of blanks ignored (SQL Server keeps `CREATE OR ALTER VIEW` as
   `CREATE   VIEW`), its own name left out, and every name its own schema qualifies counted as
   the same on both sides (PostgreSQL, MySQL and DuckDB write a view's own schema into its
   text, so the same view in two schemas would otherwise always differ). A foreign key's
   `ON DELETE` and `ON UPDATE` are not compared: the driver contract does not report them yet,
   and the report says so on every foreign key.
   Every name is found as a token by the tokenizer the structure forms already use for
   `CREATE TABLE` text (ADR-047; the dialect's scanner once ADR-065 lands), never by a text
   search. Moving a definition to the target replaces only its own name (and an index's own
   table) and the source schema where it qualifies a name (`src.t` becomes `dst.t`): those
   are positions the tokens show. The source schema anywhere else in a source text, inside a
   string (dynamic SQL) above all, is not rewritten: the report says the text names it, for
   the user to review.
5. **The report is per object**: only in source, only in target, changed, or the same (only
   counted). A changed table lists each changed property side by side (a column's type,
   nullability, default, comment; an index's or constraint's definition); a changed view or
   routine carries a line diff of its source, computed in the core with the line diff the MCP
   page already uses (`line_diff`, ADR-033), moved where both can reach it. The UI shows the
   core's report and never compares anything itself.
6. **The script makes the target like the source.** The core writes it from both snapshots,
   reusing what already writes each statement for every engine:
   - a table only in source: `CREATE TABLE` with its columns, keys and checks, from the
     structured snapshot through ADR-047's `create.rs` (which a snapshot feeds as a form
     would), in the target schema; then its indexes; its foreign keys after every table exists;
   - a changed column: ADR-047's alteration of that table, written against the target's
     catalog as the form's is (one `ALTER TABLE` on
     PostgreSQL, `MODIFY COLUMN` restating every attribute on MySQL and MariaDB, SQL Server's
     restated nullability and named defaults, SQLite's rebuild with its pragmas and checks);
     a column only in source: `ADD COLUMN`;
   - indexes and constraints added or changed: ADR-026's statements (a changed one is dropped
     and created again); foreign keys are dropped first and added last;
   - a view or routine only in source or changed: its source written as the statement that
     replaces it ("Edit source", ADR-047), in the target schema, views after the views and
     tables their text names;
   - what is only in the target (tables, columns, views, routines, indexes, constraints) is
     dropped only when the user asks for it in the comparison's options, and is otherwise
     listed in the script's header as left in place. Drops come first, in reverse dependency
     order.
   The script begins with a header saying what it was written from (both connections and
   schemas, when) and which statements cannot be undone (`DROP`, a SQLite rebuild). What the
   core cannot write for an engine (a property no form can change there) is not dropped
   silently: the object is reported as "cannot be scripted here", with the reason, and the
   script says so in a comment.
7. **The script is never run from the comparison.** "Open script" opens it in a new editor tab
   connected to the target's connection and schema (ADR-026), where read-only connections refuse
   it (ADR-053), the guard asks about its drops and the connection's type confirms what it
   confirms. Running it is the user's, statement by statement or as a script (ADR-035).
8. **Done means a second comparison is empty.** For each engine, the tests build two schemas
   with known differences of every kind above, compare them, run the script on the target, and
   compare again. Two round trips, each a test of its own: without drops, the second report
   holds only what is only in the target; with drops, the case that can destroy data, it holds
   nothing at all, and the target's rows in the tables both sides keep are still there.

## Alternatives
- **Compare the engines' own DDL text (`relation_definition`) instead of structured
  snapshots:** simple to show, but every engine writes defaults, constraint order and
  qualifiers its own way, so equal tables would differ, and a script cannot be written from a
  text diff.
- **One snapshot format for every engine, compared generically:** what the forms already
  know per engine (MySQL's restated attributes, SQL Server's named defaults, SQLite's rebuild)
  would be written again, worse. The comparison reuses those writers.
- **Run the script from the comparison, as the forms apply theirs:** a form applies one
  table's change it just read again; a comparison's script spans a schema read minutes ago,
  on another connection. The editor, where the guard and the connection's type apply and
  statements can be run one at a time, is the safer place (ADR-026).
- **Compare data as well:** a different question (row counts, keys, values), with its own
  cost and masking rules; a later ADR if it is wanted.
- **Rewrite every mention of the source schema's name inside view and routine text when the
  schemas' names differ:** text rewriting of SQL bodies is guesswork (strings, comments,
  dynamic SQL); only the qualifiers the tokens show are moved, and the report points at the
  rest.
- **Leave even the qualifiers as they are:** the script would create the target's views over
  the source schema's tables, and on PostgreSQL, MySQL and DuckDB a second comparison could
  never be empty when the schemas' names differ.

## Consequences
- `tests/schema_compare.rs` runs on every engine of point 1, with both round trips of point 8.
- The comparison adds no driver method: a plugin engine joins when its structure forms do.
- Big schemas take one `describe_table` per table on each side; the job shows progress and can
  be cancelled, and nothing is held in the UI but the report.
- Server objects (roles, users, tablespaces), sequences, triggers, types, partitions and
  privileges are not compared yet; each can be added with the statements that write it.
- Foreign key actions, identity and auto-increment details, collations and other attributes
  the contract does not report (`ColumnInfo` has none of them; MySQL's and SQLite's kept
  `CREATE TABLE` text has some) are not compared yet. Each needs the contract or the snapshot
  to carry it first; until then a second comparison can be empty while they still differ, and
  the report's header says which properties it does not compare.
