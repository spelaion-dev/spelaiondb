# ADR-058: SQLite over the network: libSQL is a plugin that takes SQLite's catalog and typing from this repository's library

- **Status:** Accepted (the library is implemented, SDB-314; the libSQL plugin is not yet)
- **Date:** 2026-11-15
- **Builds on:** ADR-005 (driver contract, capabilities), ADR-006 (SQLite's dynamic typing),
  ADR-018 (SQLite on a server), ADR-020 (each engine's barriers), ADR-041 (engines as plugins,
  and "SQLite stays compiled in"), ADR-042 (plugin repositories, the library by tag)

## Context
libSQL (the server `sqld`, and Turso, which hosts it) and Cloudflare D1 are SQLite's SQL and
types behind a network service, reached over HTTP instead of a file (SDB-172). Checked against
the official `libsql-server` image (sqld 0.24.33, SQLite 3.47):

- **The wire** is Hrana over HTTP: `POST /v2/pipeline` with statements, answered with columns
  (name and declared type) and rows whose every value carries its SQLite storage class
  (`integer`, `float`, `text`, `blob` in base64, `null`). A `baton` returned with each answer
  keeps a stream open across requests, so a stream is a session: a transaction can span
  requests, and a stream left idle expires.
- **No session barrier:** sqld refuses `PRAGMA query_only` ("unsupported statement"). What it
  offers is server-side access control: tokens (JWT) can be limited to reading. SQLite has no
  read-only transaction to begin with.
- **D1** is reached through Cloudflare's REST API (an account, a database id and an API token);
  there is no local server that answers that API, so nothing here can test it.

Two decisions already made pull against each other. ADR-041 says new engines are only written
as plugins, in repositories of their own (ADR-042). ADR-041's note of 2026-11-14 keeps SQLite
compiled in because its catalog queries and its dynamic typing (ADR-006, about 700 lines in
`spelaion-driver-sqlite`'s `catalog.rs` and `exec.rs`) are shared by the local file and SQLite
on a server, and "must stay identical": a copy in another repository is what it refused. A
libSQL plugin needs exactly that code: the same catalog over `sqlite_schema` and the
`pragma_*` functions, and the same types decided from storage classes and declared types.

## Decision
**libSQL is an engine of its own (`engine = "libsql"`), served by a plugin in its own
repository (`spelaiondb-plugin-libsql`). SQLite's catalog queries and typing move out of the
SQLite driver into a small library of this repository, published with the plugin SDK's tag
like the dialects, and used by the compiled-in SQLite driver, by SQLite on a server and by the
libSQL plugin, so the three cannot drift apart.**

1. **The library** (`spelaion-sqlite-schema`, in this workspace): the catalog's SQL and the
   reading of its rows into the contract's types, and the typing of a result from rows of
   storage-class values plus declared types into Arrow batches with `spelaion.logical` and
   `spelaion.native_type` (ADR-005, ADR-006). It depends on the driver contract and Arrow, not
   on `rusqlite`, the network or a runtime: a transport hands it values and gets batches back.
   `spelaion-driver-sqlite` (both of its transports) is rewritten on top of it in the same pull
   request, its tests unchanged. Plugins take it by the SDK tag (ADR-042, point 2), which makes
   it part of what a tag pins.
2. **The plugin** speaks Hrana over HTTP. A session is a stream: its baton is kept, a
   transaction can be opened on it, and a stream the server expired marks the session lost
   (ADR-028); nothing is re-run. It declares what there is (ADR-005): transactions and cancel
   only as far as the stream allows (cancel closes the request), no `insert_rows` until a bulk
   path is written.
3. **The connection** is a host, a port and TLS like the other servers, and the token is its
   password: in the keychain, sent only in `connect` (ADR-041). The core opens tunnels and
   proxies as for every engine and gives the plugin `connect_addr`; the plugin's HTTP client
   connects there, verifies TLS for the host, and reads no proxy from the environment, so a
   configured proxy is never bypassed by a direct connection.
4. **No read-only mode nor agents** in the first version: there is no session barrier, and a
   read-only token is the server's to enforce, which the core cannot check without writing. An
   item of its own decides how a read-only token is recognised (its claims, or the server's
   answer to a request that cannot change anything) before either is offered (ADR-020).
5. **D1** waits for an account to test against. When it comes, it is a second engine of the
   same plugin if the wire code stays small next to what they share (the library, the HTTP
   client), and a plugin of its own otherwise.

## Alternatives
- **Copy SQLite's catalog and typing into the plugin.** The least work now, and two copies that
  must give the same explorer and the same types, in two repositories, which ADR-041's note on
  SQLite refused for exactly this code.
- **libSQL as a third transport of the compiled-in SQLite driver,** as SQLite on a server is
  (ADR-018): no new repository and no tag, one copy of the code. It breaks "new engines only as
  plugins" and puts an HTTP database client into the core, which ADR-041 took out of it.
  **Choose it instead if** the library cannot be made free of `rusqlite` without bending the
  local driver.
- **Turso's `libsql` crate as the plugin's client.** It may carry the wire, but it brings its
  own SQLite build and its own idea of types; the typing stays the library's. Decided while
  writing the plugin, by size and by whether its HTTP connector can be pointed at
  `connect_addr` with no environment proxy.

## Consequences
- The plugin cannot be built until a plugin SDK tag carries the library: one pull request here
  (the library, the SQLite driver on top of it), a tag, then the plugin's repository and its
  pin.
- The SQLite driver's code moves without changing behaviour; its type zoo and every engine
  test that runs SQLite are the check.
- `Engine` gains `libsql` (configuration, schema, the form's fields, the picker), as each new
  engine does.
- libSQL users get the editor, results, the explorer and edits, without read-only mode or
  agents until the token item lands.

## Revisit if
- The library turns out to need `rusqlite` to type values the way the local driver does: the
  alternative of a transport in the core.
- Turso's protocol replaces Hrana over HTTP with something the plugin cannot keep a session on.
