# ADR-024: The explorer groups objects in folders by type

- **Status:** Accepted
- **Date:** 2026-11-04
- **Refines:** point 4 of ADR-005 ("driver-driven schema") for the relational engines

## Context
The explorer showed one flat list per schema: tables and views mixed, with a T or V badge.
Partitioned tables, materialized views and foreign tables looked like plain tables or views,
and nothing else in a schema could be seen: functions, procedures, sequences, triggers, types.
Nor could anything outside the schemas: roles, logins, tablespaces, extensions. DBeaver
users expect all of it, in folders by type.

ADR-005 imagined generic nodes sent by each driver (`kind`, `label`, `icon`, actions). The six
engines we have are relational, and their objects fit a short list of kinds that the UI can
name, order and translate. Generic nodes stay for the first engine that does not fit.

## Decision
1. **Two typed listings in the driver contract**, next to `list_objects` (which keeps the
   relations: the table data tab, editing and the MCP use it):
   - `list_schema_objects(schema)`: functions, procedures, aggregates, sequences, triggers,
     enums, domains, other data types, events (MySQL) and synonyms (SQL Server).
   - `list_server_objects()`: what lives outside every schema, on the server or in the
     database the connection is in: roles, users, logins, tablespaces and extensions.
   Each item has a name, a kind (an enum: the UI matches it exhaustively), an optional
   `detail` and an optional comment. `detail` is shown next to the name and tells same-name
   objects apart: a PostgreSQL or DuckDB function's arguments, a trigger's table.
2. **`object_definition(object)`** returns the object's DDL, as the engine writes it where it
   can (`pg_get_functiondef`, `pg_get_triggerdef`, `SHOW CREATE …`, `OBJECT_DEFINITION`,
   `sqlite_schema.sql`) and composed from the catalog otherwise (sequences, PostgreSQL types
   and aggregates, SQL Server types and synonyms, DuckDB macros). The app opens it in a new
   tab; nothing runs.
3. **The folders are the app's**: under each schema, "Tables" (partitioned ones marked P),
   "Foreign tables", "Views", "Materialized views", then the kinds of `list_schema_objects`
   (enums, domains and other types share "Data types"); under the connection, after its
   schemas, the kinds of `list_server_objects`. Only folders with something in them are
   shown, with their count.
4. **What each engine lists:**

   | Engine | In a schema | Outside the schemas |
   |---|---|---|
   | PostgreSQL | functions, procedures, aggregates, sequences (not those of identity columns), triggers, enums, domains, composite and range types | roles (not the predefined `pg_*`), tablespaces, extensions |
   | MySQL, MariaDB | functions, procedures, triggers, events; sequences (MariaDB) | users and roles (MariaDB), when `mysql.user` can be read |
   | SQL Server | functions, procedures, sequences, triggers, alias and table types, synonyms | logins, database users and roles |
   | SQLite | triggers | — |
   | DuckDB | macros (as functions), sequences, enums and other types | — |

   An object that belongs to a PostgreSQL extension is left out: the extension stands for
   it (otherwise PostGIS alone puts more than a thousand functions in `public`).
5. **All of this is read-only catalog queries in the drivers**, with names bound as parameters
   or quoted by the engine (`format('%I')`, `QUOTENAME`) or by our dialect. Nothing here changes
   the barriers of ADR-008, ADR-019 or ADR-020, and the MCP does not offer these listings yet.

## Alternatives
- **Generic nodes from each driver (ADR-005's idea):** every engine would carry its own labels
  and icons, and the UI could not translate nor order them. Worth it for engines whose objects
  are not relational; not for these six.
- **One call per folder, when it is opened (as DBeaver does):** shows empty folders, and each
  opening is a round trip through a tunnel. The listings here are cheap catalog queries, so the
  schema loads them once.
- **Showing extension members (as `psql`'s `\df` does):** faithful, but buries the user's own
  functions.

## Revisit if
- A schema lists so many objects that loading them at once is slow: then one call per folder.
- Agents ask for routines or triggers: then an MCP tool over these listings.
- Partitions should hang from their parent table (they are hidden from the list today).
