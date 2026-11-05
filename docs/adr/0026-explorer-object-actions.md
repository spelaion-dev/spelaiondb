# ADR-026: The explorer's object actions: SQL from the core, run only after it is shown

- **Status:** Accepted
- **Date:** 2026-11-05
- **Builds on:** ADR-024 (the explorer's folders), ADR-025 (explorer filters)

## Context
DBeaver's navigator does more than read: it copies an object's name or qualified name,
renames and drops objects (showing the SQL first, with a cascade option), creates new ones,
adds an index or a constraint on the chosen columns, opens an object by name from anywhere
(Ctrl+Shift+D), sets the editor's active schema, follows the active editor in the tree and
keeps bookmarks. Some of these change the database, so they need the same care as the
editor: the user must see what runs, read-only connections must stay read-only, and the
quoting must be the engine's.

## Decision
1. **The core writes every statement** (`spelaion-core::objects`), with the engine's quoting
   and syntax (`ALTER TABLE … RENAME TO`, `RENAME TABLE`, `sp_rename`, `DROP … CASCADE`, `DROP
   INDEX … ON`…). What an engine cannot do is not offered: the app asks the core which actions
   the session has, never the engine's name. The UI sends what to do (the object and the
   options), never SQL to run.
2. **Creating opens SQL, it does not run it.** "Create" (a table, view, materialized view,
   schema…) and "New index / constraint" on the chosen columns open a statement in a new tab,
   like "Generate DDL" (ADR-024): the user names things and runs it in the editor, where its
   usual confirmations apply.
3. **Renaming and dropping show the SQL first.** A dialog shows the exact statements from the
   core (with "Cascade" where the engine has it) and offers to run them or to open them in the
   editor. Running sends the same request again: the core writes the statements again, refuses
   a read-only connection, refuses without the user's confirmation (the dialog is it, on every
   connection, because a drop cannot be undone), and runs them on the explorer's session in
   order, stopping at the first error. The tree reads the schema again afterwards.
4. **Copy** puts the names as the tree shows them, or qualified as the core writes them in SQL
   (`schema.table`, `schema.table.column`), one per line for several chosen objects.
5. **Open an object by name** searches the connections already connected in the explorer,
   with `list_*_matching` and a `*text*` pattern, so big catalogs are narrowed in the catalog
   query (ADR-025), in the schemas the explorer shows. The search stops at a fixed number of
   results.
6. **Active schema:** the core sets the schema of an editor tab's own session: `search_path`
   on PostgreSQL (the schema, then `public`), the database on MySQL and MariaDB, `SET schema`
   on DuckDB. SQL Server and SQLite have no schema per session, so it is not offered there. The
   core also reads a tab's current schema, which "Link with editor" uses.
7. **Link with editor** shows the active tab's connection, schema and table in the tree. It
   only expands what is already connected in the explorer: it never connects on its own,
   because connecting may ask for a password or a trust decision.
8. **Bookmarks are personal**, kept in `state.db` per workspace and connection like the
   explorer filters (ADR-025): an object by its schema, name and kind. A bookmark whose object
   is gone stays until it is removed, and opening it says so.

## Consequences
- An object action can never run SQL the user has not seen, nor anything else than what the
  core writes for the object chosen.
- Each engine's statements are tested by running them on that engine
  (`spelaion-core/tests/object_actions.rs`).
- Renaming covers relations and columns, dropping covers what the tree lists in a schema
  (and schemas themselves where the engine has `DROP SCHEMA`/`DROP DATABASE`). Server objects
  (roles, logins, extensions) are left out for now.
