# ADR-074: A PostgreSQL connection may open the other databases of its server: opt-in, a session per database, one set of settings, and agents keep to the connection's own database

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Builds on:** ADR-009 (one file per connection), ADR-019 (agents, per connection), ADR-022
  (trust pinned to content), ADR-029 (safeguards only add up), ADR-053 (read-only
  connections), ADR-061 (a result belongs to the session and database that read it)
- **Amends:** ADR-024 (the explorer of such a connection has a database level), ADR-037 (the
  default schema applies in the connection's own database)
- **Decides:** SDB-45

## Context
A connection is one file with one `database`. On MySQL, MariaDB and SQL Server that is only
where a session starts: a tab changes database on the same session
(`Connection::select_database`), the explorer or the tab's picker lists the server's databases
(`list_databases`), and the file's `databases` narrows what is shown (SDB-396, `shown.rs`).
A PostgreSQL session cannot leave its database, so `Engine::chooses_databases` is false for
it, `databases` is refused in its files (`load.rs`) and nothing in this repository lists a
PostgreSQL server's databases. A server with one database per tenant, or an administrator's
view of a server, needs one connection file per database: one more file, trust pin, keychain
entry and explorer row each. `psql`, pgAdmin and DBeaver ("Show all databases") all show the
server's databases under one connection.

What the code already has, checked on `dev`:

- **A session per tab**, plus one explorer session per connection (`meta:<id>`), completion's
  and each streaming export's (`AppState.sessions`, keyed by tab). A session is opened from
  `ConnectParams`, whose `database` is one name; the plugin protocol's `Connect` carries it.
- **The route does not depend on the database.** `TunnelPool`'s key is the layer, the tunnel's
  name, its settings and its hops; forwards are shared per target host and port
  (`Tunnel::forward_on`); a Kubernetes forward is keyed by its resource and port. Two databases
  of one server go through the same SSH chain and the same forward today
  (`tests/open.rs`).
- **A session made from another one** (`session_source.rs`, SDB-426) reuses a typed password
  when the login is the same: engine, host, port, user and route. The database is not part of
  that comparison.
- **Stores.** History already records the database (`history.database`) and the editor's
  completion cache is keyed by it. Explorer bookmarks and table filters are keyed by workspace,
  connection, schema and name; restored tabs keep a connection id and no database; the audit
  log keeps a connection id.
- **Trust** pins every field of the file but its name (`connection_snapshot`), so `database`
  and `databases` are pinned, and a new key would be.
- **Agents** name a connection by its id; no MCP tool takes a database, and the MCP server's
  sessions are keyed by connection and purpose.

## Decision
1. **Yes, and only where the file says so.** A PostgreSQL connection file may name the other
   databases it opens: `databases = ["billing", "reports"]` (the key MySQL, MariaDB and SQL
   Server already have), or `all_databases = true` for every database the server lists; one or
   the other, never both. **Without either, a PostgreSQL connection is its `database` and
   nothing else**, as today: no existing file changes meaning, and no trust snapshot changes.
   On PostgreSQL the list is not only what is shown, as it is on MySQL: it is what the core
   opens under that connection. The core refuses a session on a database the file does not
   name. It is still not a permission of the server's; a read-only database user stays the
   only real guarantee.
2. **A database opened under a connection is that connection.** There are no per-database
   settings. `read_only`, the connection type and its safeguards, masking, `[mcp]`, the
   session setup, TLS, timeouts, the keep-alive and the route are the file's, and apply to
   every database the same. So a database cannot get a looser setting than the connection it
   came from: it has no setting of its own to be looser. A database that needs different
   settings gets its own connection file, as today.
3. **A database that has a connection of its own opens only as that connection.** When another
   connection of the same folder, or a personal one, has the same login (engine, server, user
   and route: what `session_source::same_login` compares) and names that database as its
   `database`, the core refuses to open it under a different connection, and the explorer's row
   leads to its own connection. Otherwise `all_databases` on a Development connection would be
   a way into the production database of the same server without Production's safeguards or
   its read-only mode.
4. **Opening another database is a new session, never a switch.** The core builds it from the
   connection's resolved definition with only `ConnectParams.database` changed: same host,
   user, password, TLS and route. `select_database` stays unsupported on PostgreSQL. Everything
   that happens when a session opens happens for it: the engine's read-only mode (ADR-053),
   the isolation level and the setup statements (ADR-037), the server family check, the type's
   idle close (ADR-029).
   - **The default schema** (`[session] schema`) is set only in the connection's own
     `database`: ADR-037 fails a session whose schema did not take, and another database need
     not have a schema of that name. A session on another database starts with the server's
     `search_path`. Setup statements run in every database; one that fails there fails that
     session, as ADR-037 says.
   - **In an editor tab** the database is chosen when the tab connects (from a database's row in
     the explorer, or the tab's picker). Changing it on a connected tab is a disconnect and a
     connect to the other database: refused while a script runs, asked about like any
     disconnect while a transaction is open (ADR-031), and nothing the user ran is run again.
     Results the old session read stay on screen and are no longer editable (ADR-061).
5. **Tunnels: nothing new.** Every database's sessions go through the connection's one route:
   the same `TunnelPool` entry and the same forward, since the target is the server's host and
   port. A session per database is a server connection per database; the explorer opens a
   database's session when its row is first expanded, not before, and closes it with the
   connection or from the row.
6. **The server's list comes from the driver contract's `list_databases`**, which the
   PostgreSQL plugin implements for this (in its own repository, ADR-042): the databases that
   accept connections and that the user may connect to, never templates. The core keeps only
   the file's `databases` when it has a list, and always the connection's own. A plugin that
   does not answer leaves the connection with its own database and says why.
7. **Where it does not apply.** Only a server that is PostgreSQL itself
   (`ServerFamily::PostgreSql`): the other servers of its protocol keep one database until each
   is checked. Not with a `pooler`: a pooler routes by its own list of names, which is not the
   server's. Both are refused when the file loads or the session opens, never quietly ignored.
8. **Agents keep to the connection's own database.** `list_connections` is unchanged, no MCP
   tool gets a database argument, and the MCP server's sessions open the file's `database`
   only, whatever `databases` or `all_databases` say. `mcp.exposed` is the user's decision
   about one database; with `all_databases` it would widen by itself whenever someone creates
   a database on that server. A database agents should reach gets a connection file of its
   own, with its own `[mcp]` (ADR-019).
9. **Identity does not change.** A connection is identified by its folder and its id (ADR-075
   says how when several folders are open); a database is never part of the id, and no id is
   built from a database's name. What belongs to one database carries the database next to the
   connection, as history already does:
   - **history** and the completion cache: as they are;
   - **explorer bookmarks, table filters and restored tabs** gain the database (empty for the
     connection's own, so existing rows keep their meaning). A restored tab whose database is
     no longer one the connection opens comes back disconnected, with a note;
   - **explorer filters, editor variables, saved queries, the history switch, the agents'
     read-only override and the keychain entry** stay per connection: one password, one set of
     filters;
   - **the audit log** needs nothing: agents have one database.
10. **Trust.** `databases` and `all_databases` are fields of the file, so ADR-022 pins them:
    a pull that widens a repository connection to other databases is shown and accepted before
    the connection resolves again. Tests try to get around it: a changed list, a database
    outside the list, a database with a connection of its own, a pooler, a server that is not
    PostgreSQL.
11. **The explorer (amends ADR-024, for these connections only).** Under the connection: its
    databases, its own first, each with its schemas and their folders, and with its
    extensions, which belong to a database. Roles and tablespaces, which belong to the server,
    stay under the connection, read from its own database's session. A connection that names no
    other database keeps today's tree, with no database level.
12. **Importing.** A client's "show all databases" is not turned into `all_databases`: the
    import lists it as not mapped (ADR-039), and the user turns it on in the form.

## Alternatives
- **One connection per database, made easy** (the most reasonable alternative): an action that
  adds a connection for each of the server's other databases, built on duplicating (ADR-030).
  Nothing in the model changes and every database keeps its own settings and its own agents'
  access. But a server with forty tenants is forty files, pins and explorer rows, and a tenant
  created tomorrow is not there. It remains the way for databases that need different
  settings.
- **Per-database settings inside one file** (a section per database). Each setting would need
  its own rule for combining with the connection's so that nothing loosens, the trust review
  would show a matrix, and "which read-only applies here" would stop having a one-line answer.
- **On for every PostgreSQL connection, as pgAdmin does.** A repository connection the user
  trusted for one database would reach others after an app update, with nothing to review.
- **Each database becomes a connection of its own to agents**, or a `database` argument on the
  MCP tools. Exposure would then be decided by the server's catalog, not by the user.
- **An id per database** (`orders#billing`). Every store, the keychain and the trust store
  would treat a database as a connection, and ids would have to be parsed.
- **Switching the tab's session in place.** PostgreSQL has no such thing; pretending it does
  would hide that the session, its transaction and its settings are gone.

## What would change this
- Users who need a different read-only mode or type for some databases of one connection:
  per-database settings, each with a tighten-only rule, in an ADR of its own.
- Agents that must reach several databases of one server: a list of names in `[mcp]`, written
  by the user and pinned by trust; never "all".
- If servers with very many databases make a session per expanded database a problem for the
  server's connection limit, the explorer shares one session per database between its row and
  the tabs' metadata.

## Work items if accepted
- The PostgreSQL plugin lists the server's databases (`list_databases`), and its pin (S).
- `databases` and `all_databases` on PostgreSQL: the model, the load rules of point 7, the
  form and trust tests (S).
- The core opens a session on another database of the connection, with points 1, 3, 4 and 7
  and tests against a real PostgreSQL that try to get around them (M).
- The explorer's database level and where server objects hang (M).
- The tab's database on PostgreSQL: the picker as disconnect and connect, the saved tab's
  database, and the database in bookmarks and table filters (M).
- What is opened for one database from the explorer takes that database: backup and restore
  (ADR-049), the data search, ER diagrams, privileges (M).
