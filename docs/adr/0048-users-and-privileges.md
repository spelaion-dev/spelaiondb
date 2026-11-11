# ADR-048: Users and privileges: forms that write reviewed SQL, with the password kept out of everything but the statement

- **Status:** Accepted
- **Date:** 2026-11-10
- **Builds on:** ADR-026 (the explorer's object actions), ADR-047 (structure editing with
  forms), ADR-008 (read-only mode), ADR-033 (the MCP activity log)

## Context
Creating an account and giving it access is a common admin task whose syntax few people
remember, and each engine does it differently: PostgreSQL has roles that may log in, MySQL and
MariaDB have `'user'@'host'` accounts and roles, SQL Server has server logins mapped to users
of each database, and database roles. ADR-026 left server objects out of the explorer's
actions.

These forms carry something the other forms do not: a new password. AGENTS.md says a secret is
never written to logs, configuration, history or MCP responses. The structure forms of ADR-047
show the script and apply only the script that was shown, comparing the two as text, so the
password cannot simply be part of the shown script.

## Decision
1. **The same pattern as ADR-047.** The form sends what to do, never SQL
   (`spelaion-core::objects::users`): create a role, user or login, change its password, lock or
   unlock it, drop it. The core writes the statements with the engine's syntax and quoting, the
   dialog shows them, and Apply sends the form again with the script shown. The core writes the
   script again and refuses it if it differs, refuses a read-only connection and a session with
   a transaction open, and runs a drop only confirmed (the dialog is the confirmation, as in
   ADR-026). What an engine cannot do is not offered: the app asks the core which kinds and
   actions the session has.
   - PostgreSQL: roles (`CREATE ROLE … LOGIN PASSWORD`, `CREATEDB`, `CREATEROLE`,
     `SUPERUSER`); locking a role is `NOLOGIN`, unlocking it `LOGIN`.
   - MySQL and MariaDB: users (`CREATE USER `name`@`host` IDENTIFIED BY`, `ACCOUNT LOCK` /
     `ACCOUNT UNLOCK`) and roles (`CREATE ROLE`).
   - SQL Server: logins (`CREATE LOGIN … WITH PASSWORD = N'…'`, default database,
     `CHECK_POLICY`; locking is `DISABLE`, unlocking `ENABLE`), users of the connection's
     database (`FOR LOGIN` or `WITHOUT LOGIN`, default schema) and database roles. *(Extended
     later: a contained database's user, `CREATE USER … WITH PASSWORD = N'…'` and `ALTER USER …
     WITH PASSWORD`, whose password follows every rule below. Windows logins, `CREATE LOGIN …
     FROM WINDOWS`, are not offered: the Linux test server cannot resolve a Windows account.)*
2. **The shown script never holds the password.** The core writes it with `'***'` in place of
   the password literal (`N'***'` on SQL Server). The form says only whether a password is set;
   its value is sent once, on Apply, as its own argument, never as part of the form, and the core
   holds it as a `Secret`. Apply compares the masked script it writes again with the one shown,
   then writes the same statements with the real literal and runs those. The password cannot be
   opened in the editor: a script that sets one has no "Open in editor".
3. **The real statement goes nowhere else.** It runs as the core's own statement
   (`Session::run_generated`), which the history never records, and only from the desktop: the
   MCP server has no tool for it, so it never reaches the audit. A failure's message is
   scrubbed before it leaves the core: every spelling of the password the statement held (as
   typed, with its quotes doubled, with MySQL's backslashes escaped) becomes `***`, then the
   audit's own masking runs on it. The audit's and history's masking also learns SQL Server's
   `PASSWORD = N'…'`, so a password typed in the editor is masked there too.
4. **A password is written as one string literal or not at all.** Quotes are doubled; MySQL and
   MariaDB escape backslashes, unless the session's `sql_mode` has `NO_BACKSLASH_ESCAPES`, and
   PostgreSQL keeps them, unless `standard_conforming_strings` is off: in those two modes a
   password with a backslash is refused, because the engine and the core's statement splitter
   would read it differently. Every real statement must split into exactly one statement, or
   nothing runs. A password cannot be empty or hold a NUL.
5. **Privileges follow the same rules** (`spelaion-core::objects::privileges`): a form grants or
   revokes privileges globally, on a database, on a schema or on one object, and the core writes
   `GRANT` / `REVOKE` with the engine's names for each level and only the privileges that level
   has. What an account has is read from the catalog by the core (PostgreSQL's ACLs and role
   memberships, MySQL's `SHOW GRANTS`, SQL Server's permissions and role memberships) and shown
   in the same dialog. *(Extended later: some columns of a table, `GRANT SELECT (a, b) ON t`, on
   every engine, with only the privileges each has on columns; and PostgreSQL's default
   privileges, `ALTER DEFAULT PRIVILEGES [FOR ROLE r] [IN SCHEMA s] GRANT … ON TABLES`, for
   tables, sequences, functions and types created later, read back from `pg_default_acl`.)*
6. **Where they are:** the explorer's Roles, Users and Logins folders have "New role…", "New
   user…" and "New login…" in their menu, and each account has "Change password…", "Lock or
   unlock…", "Privileges…" and "Drop…" where its engine has them. Those folders show on
   connections whose engine has accounts even when the session cannot read any.

## Consequences
- Each engine's statements are tested by running them on that engine
  (`spelaion-core/tests/users.rs`): an account is created, logged in with its password, given a
  new one, locked, unlocked and dropped.
- A test creates an account and changes its password, then reads the history, the audit and the
  store's file on disk and the errors the core returned, and finds the password in none of them.
- The password is still visible to the server itself: in its process list while the statement
  runs, and in whatever statement log the server keeps. That is the server's configuration, not
  the client's.
- An account whose name the server lists but the session cannot change (no privilege) fails when
  applied, with the server's message.
