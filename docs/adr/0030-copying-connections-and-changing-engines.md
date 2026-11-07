# ADR-030: Copying connections, changing their engine, and naming new ones

- **Status:** Accepted (implemented)
- **Date:** 2026-11-07
- **Builds on:** ADR-009 (configuration and secrets), ADR-022 (team sharing)

## Context
DBeaver's navigator and connection editor have three things people use daily that SpelaionDB
lacked:

- **Copy and paste a connection** ("Advanced copy/paste"): the fastest way to make the staging
  twin of a production connection. A copy touches the three things the configuration is most
  careful with: where a connection lives (personal or the team's repository), its password in
  the keychain (entries are keyed by the connection's identifier, ADR-009), and repository
  trust (ADR-022 pins each repository connection to what the user reviewed).
- **Change a connection's driver**, keeping its settings: MySQL ↔ MariaDB, or a database
  moved to another engine. The edit form fixed the engine once created.
- **A pattern for the name of new connections** (`DEFAULT_CONNECTION_NAME_PATTERN`). The form
  left the name empty, filled it from a pasted URL's database or host, and from a new file's
  name.

## Decision
1. **Duplicate is done by the core** (`spelaion-config::duplicate_connection`), never by the UI:
   - The file is copied **next to the original: same layer, same folder**, as
     `<file>-copy.toml` (then `-copy-2`…) named "<name> (copy)" (then "(copy 2)"…). The text is
     copied as it is, with comments and keys the form does not show; only `name` changes. A
     file that already exists is never replaced (it is created with `create_new`).
   - **A team connection's copy stays in the repository.** Its relative paths (a SQLite file,
     `tls.ca`) mean the same there, and the team sees it in review like any new file. The
     original must be as the user trusted it: an untrusted repository, or a connection that is
     new or changed since it was reviewed, is refused with the same "trust" or "review the
     change" answer as opening its editor. The copy is then recorded as the user's own write
     (`accept_connection_write`), exactly as saving a new connection from the form is: it holds
     nothing the user had not already accepted.
   - **The keychain password is copied inside the core**: read from the original's entry and
     written to the copy's own entry, so the copy connects like the original and deleting one
     never breaks the other. The value never reaches the UI, the clipboard or a file. Without a
     stored password, or without a system keychain at all, the copy asks for it like a new
     connection, and an entry left under the copy's name by an older connection is deleted so
     it is never sent to the copy's server. A keychain that exists but fails (locked, refused)
     is an error, and no copy is made. A named entry (`keychain:<name>`) is shared on purpose and is kept as it
     is; `env:`, `op://`, `cmd:` and `prompt` are references and are copied as such.
2. **Ctrl+C / Ctrl+V on a connection** in the explorer use a text form (`connection_clipboard_text`):
   the connection file without anything that reads a secret, runs a program or lets agents in.
   The password becomes `prompt`; `kubernetes.kubectl`, `kubernetes.kubeconfig` (a kubeconfig can
   run `exec` credential plugins), `sqlite3` and `[mcp]` are left out, and the paths of a team
   connection become absolute. Copying a team connection needs the same trust as duplicating it.
   - **Paste treats the text as untrusted** (it may come from a chat or a web page): it creates
     a **personal** connection, never a repository one, through the connection form's own
     checks (`ConnectionDraft` → `add_connection`), so it can never hold what the form would
     refuse. Whatever the text says, the password is asked when connecting (no keychain, `env:`,
     `op://` or `cmd:` is read, no cloud-token command runs); the program-running keys and
     `[mcp]` are dropped; `${env:…}` is refused anywhere, because resolving it would send an
     environment variable to the server, or to DNS through a host name; a tunnel is kept only if
     one of that name exists here. The user is told what was left out, and the editor opens on
     the new connection. A connection URL (`postgres://…`) pastes the same way, without its
     password.
   - Ctrl+C keeps copying object names on object rows (ADR-026): only connection rows copy a
     connection.
3. **Changing the engine** is part of editing. The core maps the form
   (`ConnectionDraft::with_engine`) and says what is kept, changed, dropped and left to fill
   in; the form shows that before saving, and saving is an ordinary edit (only the changed keys
   are rewritten with `toml_edit`, and a repository connection goes through trust like any
   edit). The rules:
   - Between servers everything stays (host, user, password reference, database, tunnel and
     local port, Kubernetes, TLS, keep-alive). The port follows the engine when it was the old
     engine's default and stays when it was a port of its own. SQL Server takes neither client
     certificates nor cloud tokens (ADR-022): they are dropped and the password is asked.
   - A server and a file engine mean different things by `database`: it is cleared and must be
     filled in. Leaving a server drops every server setting; reaching one asks for the host and
     user, and starts with a keychain password and the suggested keep-alive, as a new connection
     does.
   - SQLite ↔ DuckDB keep the file, except a SQLite file on a server (ADR-018), whose path,
     tunnel and `sqlite3` mean nothing to DuckDB.
   - The name, folder, environment, read-only mode, agents' access and any setting not listed
     here stay: the mapping starts from a copy of the form, so settings added later are kept by
     default.
   - The connection's identifier does not change, so its filters, bookmarks and history stay.
     Its open sessions belong to the old engine: the app closes them on saving (tabs stay, and
     connect again with the new engine when run).
4. **The name pattern is a personal preference** (`personal_settings`, key
   `connection_name_pattern`), checked by the core before it is stored. Variables are `{host}`,
   `{port}`, `{database}`, `{user}`, `{engine}`, `{env}` and `{file}` (a file engine's file
   name; `{database}` is then the name without extension); `{a|b}` takes the first with a value;
   `{{`/`}}` are braces. An unknown variable is an error that names it. **The default is
   `{database|host}`**, what the form did before. The add-connection form fills the name from
   it as fields change until the user types a name (emptying the field gives it back to the
   pattern); an empty pattern suggests nothing. Editing and the DBeaver import keep their
   names.

## Alternatives
- **The copy of a team connection goes to the personal connections.** It would keep the
  repository untouched, but relative paths would break, and the copy of a team connection
  would silently become a personal one that needs no trust. Revisit if teams find copies
  committed by mistake; the explorer could then ask where to put it.
- **The copy asks for its password** instead of copying the keychain entry. Simpler, and one
  less write to the keychain, but the copy would not connect like the original, which is what
  people expect of "duplicate".
- **The clipboard holds a connection URL.** URLs cannot carry a tunnel, Kubernetes or most
  options, so a paste would lose them; the TOML text keeps everything that is safe to share.
- **The engine is chosen again from the catalog, in a new form.** That is "add connection"
  with extra steps; the point is keeping what still applies.

## Consequences
- Duplicating a connection that uses the keychain writes one keychain entry. The smoke test
  only duplicates connections whose password is asked, so it never touches the real keychain.
- Pasting cannot reproduce a connection exactly: it is a safe starting point that opens in the
  editor. Duplicate is the exact copy.
