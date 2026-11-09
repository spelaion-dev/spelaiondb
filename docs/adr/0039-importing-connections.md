# ADR-039: Importing connections from other clients: one flow, a reader per client

- **Status:** Accepted (implemented)
- **Date:** 2026-11-08
- **Builds on:** ADR-009 (configuration and secrets), ADR-022 (team sharing), ADR-027 (network
  profiles), ADR-030 (copying connections)

## Context
Someone trying SpelaionDB decides in the first hour, and retyping thirty connections is a reason
to give up. DBeaver's import existed (`dbeaver.rs`), but its flow lived half in a Tauri command:
it renamed every clash to `name (DBeaver)`, kept DBeaver's folders whether wanted or not, and
silently dropped what it could not map. Seven more clients are planned (DataGrip, TablePlus,
HeidiSQL, MySQL Workbench, pgAdmin, Beekeeper Studio, Navicat). Each keeps passwords its own way:
DBeaver obscures them with a built-in key, DataGrip puts them in the system keychain or in its
own KeePass database.

## Decision
1. **One flow in the core, one reader per client.** `spelaion-config::import` holds the model
   every reader produces (`ImportedConnection`: engine, address, user, SSH tunnel, SOCKS proxy,
   TLS, read-only, folder, description, tags, and what was not mapped), the drafts written for it
   (`import_plan`), duplicate detection, the target folder and what a replacement keeps. A
   client's module (`dbeaver.rs`, `datagrip.rs`, …) only reads that client's files. The desktop
   commands (`scan_connections`, `import_connections`) take the client's id; no secret ever goes
   to the UI, and the files are read again on import.
2. **Duplicates are found by the core, and nothing is replaced unasked.** A connection already
   exists, among the connections of where the import goes (personal or the project's), when one
   has the same name ignoring case, or reaches the same database (engine, host, port with the
   engine's default, database, user; the same file for SQLite and DuckDB). The preview names it
   and starts it left out; the user picks *import as a copy* (`name (Client)`), *replace* or
   *skip*. **Replace** edits the existing connection in place (`update_connection`, so unmanaged
   keys and comments stay, and a repository connection is re-pinned like any edit, ADR-022): what
   the client has (engine, address, user, password, TLS, tunnel) comes from the client; the name,
   folder, session setup, timeouts, agents' access and anything the client has no say on stay; a
   read-only connection stays read-only, and an empty description, tag list or type does not
   clear the existing one.
3. **Target folder.** The user picks a folder for the imported connections (or the top level)
   and whether the client's folders are kept inside it. Folder names go through the same checks
   as a typed one.
4. **Nothing is dropped silently.** Each reader lists what it does not import (driver
   properties, unknown settings, other network handlers, a password it cannot reach) as
   `what: value` lines the preview shows under the connection. A setting whose name suggests a
   secret (`password`, `token`, `key`…) shows its name only.
5. **Passwords in another application's keychain are read only on import, with consent.** A
   reader says where the client keeps a password (`KeychainItem`: service and account, e.g.
   DataGrip's `IntelliJ Platform DB — <id>`); `read_stored_passwords` reads them through
   `SecretBackend::other_app_keychain_get` only when the user imports with "Store the passwords
   … saved" checked, never while previewing. Only the macOS keychain is read, where the system
   asks the user before another app's entry is handed over: on Windows the JetBrains IDEs only
   offer their own KeePass database (DataGrip 2026.3 has no native option there), and other
   applications' Secret Service items on Linux are not searchable by our store. The password
   goes straight to SpelaionDB's own keychain entry as a `Secret`, like every other (ADR-009).
   A missing entry means the password is asked when connecting; one that cannot be read is
   reported by connection name and never stops the import. A password we cannot reach
   (DataGrip's KeePass database, its Linux keychain items) is not read: the preview says so.

## Consequences
- A new client is a reader: a module that returns `ImportedConnection`s (with `unmapped` filled
  in), a variant of `ImportClient`, a match arm in the desktop adapter's `read`, and fixtures made
  with a real install of that client. The preview, duplicates, folder, tunnels, keychain and
  report come for free.
- Reading another application's keychain entries is new for the secrets code: it is one method,
  read-only, keyed by what the other client wrote, only called from an import the user started,
  and it reads only on macOS (other applications' Secret Service items carry their own
  attributes, which the store does not search by; on Windows no JetBrains IDE writes there).
- A client's export file may be encrypted with a password the user chose (TablePlus's,
  `tableplus.rs`): the dialog asks for it, and the desktop commands take it as `file_password`
  and hand it to the reader with each read, as a `Secret`; it is never stored. A wrong password
  is refused before anything is decrypted. This is the one secret that travels from the UI, never
  to it.
- DBeaver's import changes behaviour: a clash is no longer renamed silently but left out until
  the user picks what to do with it.

## Alternatives
- **Replace overwrites the whole connection file.** Simpler, but it would drop the session setup,
  timeouts and agents' access the user set in SpelaionDB, which the client knows nothing about.
- **Read keychain passwords while previewing**, to show which ones exist. On macOS that prompts
  for each entry before the user asked for anything; the client's own files already say whether
  a password was saved.
- **Decrypt DataGrip's KeePass database** (its master key is protected with Windows DPAPI). It
  would cover DataGrip's default on Windows, at the cost of a KeePass reader and DPAPI calls;
  left for later (a Backlog item), the preview says the password will be asked.
