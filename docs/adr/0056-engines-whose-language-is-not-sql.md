# ADR-056: Engines whose language is not SQL: the core reads each language, the plugin runs what the core read, and read-only mode and agents wait for each engine's own barrier

- **Status:** Accepted
- **Date:** 2026-11-14
- **Builds on:** ADR-005 (driver contract, capabilities, `Native` values), ADR-008 and ADR-020
  (agents' layers and each engine's barrier), ADR-019 (agents' writes), ADR-029 (connection
  types and the guard), ADR-034 (streaming exports), ADR-041 (engines as plugins: dialects and
  barriers stay in the core), ADR-042 (plugin repositories, protocol versions), ADR-051
  (column masking), ADR-053 (read-only connections), ADR-055 (the Plugins page)

## Context
The engine catalogue has four families whose engines are not queried with SQL, or not only:
document databases (MongoDB and the engines that speak its protocol, CouchDB, Firestore…),
key-value and wide-column stores (Redis, Valkey, Cassandra, DynamoDB…), search engines
(Elasticsearch, OpenSearch, Solr, Meilisearch) and graph databases (Neo4j, OrientDB, Neptune);
part of the time-series family too (InfluxDB's older languages). ADR-005 said the contract
covers them (nodes the driver describes, `Native` values, capabilities), but everything that
decides what may run was written for SQL:

- a **dialect** (`spelaion-dialect`) splits a script into statements and says which parts of it
  are code;
- `classify.rs` reads each statement with `sqlparser` and decides whether it is a read; the
  **guard** (`guard.rs`) knows `DELETE` without `WHERE`, `DROP`, `TRUNCATE`;
- **read-only connections** (ADR-053) and **agents** (ADR-020) rest on that classifier plus a
  barrier the engine enforces, usually a read-only transaction;
- the core **writes SQL** for table data tabs, edits, imports, exports, column statistics, the
  data search, structure forms and object actions.

A MongoDB command, a Redis command line or an Elasticsearch request body is none of that.
ADR-041 already says that an engine with no dialect of ours gets no read-only mode and no
agents, and that a plugin is never asked whether something is a read. What it does not say is
what such an engine gets instead, what the user types, how its results reach a grid built for
rows and columns, and what the guard does with it. The owner decided (2026-11-14) that v0.10
ships these families, so the answer is needed before their first plugin is written.

## Decision
**A language that is not SQL is read by the core, as SQL is: the core splits what is typed,
turns each statement into the exact request that will run, and sends that request, never the
typed text, to the plugin. The guard asks before what the core recognises as destructive. Read-
only mode and agents are not offered on any of these engines until that engine has, in the core,
a classifier that refuses whatever it does not know and a barrier the engine itself enforces,
each with tests that try to get around it; in v0.10 no such engine has them.**

### 1. Each language belongs to the core
Every engine of ours has an entry in the core, as today's five do in `spelaion-dialect`. For an
engine whose language is not SQL that entry is a **language**: a `Dialect` (splitting, quoting,
`code_ranges`, so the editor, script runs of ADR-035, history and the cursor's statement work
unchanged) plus what is new:

- **`to_request`**: a statement turned into the engine's **canonical request**, owned data that
  can travel as a message (ADR-041 point 9). What is classified is exactly what is sent.
- **`guard_class`**: each request as `Read`, `Write`, `Destructive { why }` or `Unknown`, from
  lists compiled into the core.
- **`browse_request`**: the bounded request the explorer runs to show an object (point 5).

There are two kinds of language:

- **Languages shaped like SQL**, with statements separated by `;`: Cassandra's CQL,
  DynamoDB's PartiQL, Neo4j's Cypher, Couchbase's SQL++, Elasticsearch's and OpenSearch's SQL,
  InfluxQL. Their canonical request is the statement's text, sent as today with
  `run: statement`; what is new is their dialect and their guard lists. `sqlparser` does not
  read them, so `classify.rs` is not used for them.
- **Request languages**, whose canonical request is a structure:
  - **MongoDB's protocol:** what is typed is a closed subset of the shell's method calls,
    parsed by the core and never evaluated as JavaScript: `db.<collection>.<method>(…)` for
    `find` (with `sort`, `limit`, `skip`, `projection`), `aggregate`, `countDocuments`,
    `estimatedDocumentCount`, `distinct`, the `insert`, `update`, `replace` and `delete`
    methods, the index and collection methods, and `db.runCommand({…})` for everything else.
    Arguments are relaxed Extended JSON with the shell's constructors (`ObjectId('…')`,
    `ISODate('…')`, `NumberLong(…)`, `/regex/`). The canonical request is one command
    document: `{"database": "…", "command": {"find": "users", "filter": {…}}}`, in canonical
    Extended JSON so no type is lost. Anything the parser does not read is refused with the
    position, not guessed.
  - **Redis's protocol:** one command per line, quoted as `redis-cli` quotes. The canonical
    request is the argument list: `{"args": ["HGETALL", "user:1"]}`.
  - **HTTP request languages** (Elasticsearch's and OpenSearch's own API, Solr, Meilisearch,
    CouchDB): what is typed is a method, a path and an optional JSON body, as their consoles
    write it (`GET /orders/_search` then the body). The canonical request is
    `{"method": "GET", "path": "/orders/_search", "query": {…}, "body": {…}}`. The core refuses
    a path that is not a plain path from the root (no scheme, host, `..` or encoded `/`): the
    request goes to the connection's server and nowhere else.

A language the core cannot read is not offered: an engine's own JavaScript shell (mongosh),
Gremlin's Groovy scripts and Flux are programs, not requests, and no list of the core can say
what they will do.

### 2. The protocol carries the request (version 1.1, additive)
Within major version 1 (ADR-042 point 3), `docs/plugin-protocol.md` gains, when the first such
plugin is written:

- an `execute` run kind `{"kind": "request", "request": {…}}`, the canonical request as JSON;
  `sql` then carries the statement as it was typed, for the plugin's error messages only. **The
  plugin runs the request and never reads the typed text**: if the two disagreed, the core
  would have checked one thing and the server run another;
- optional `connect.options`, a map of strings from the connection file, whose keys the core
  checks against the engine's own list before sending (MongoDB's `authSource`, `replicaSet`,
  `directConnection`); a secret among them travels like the password, in `connect` only;
- object kinds for what is not a relation: `collection` (documents, search indexes),
  `node_label` and `relationship_type` (graphs).

`spelaion-driver-api` gets the matching `Connection::execute_request` (an owned request, the
same `EventStream`), answered by default with an error, so no SQL driver changes. A plugin that
answers `initialize` with 1.0 cannot serve an engine that needs requests: the core refuses it
and says which version it expected. The core keeps speaking 1.0 to the plugins that need
nothing else.

### 3. Results are still Arrow, with rows and columns where the data has them
The result store, windows, sorting, filters, Ctrl+F, the calc panel, copies and charts do not
change: a plugin turns what its engine returns into Arrow batches, as every plugin does.

- **Documents:** one row per document. The columns are `_id`, then the top-level fields in the
  order they are first seen in the first batch (at most 256), then a last column `…` that is
  always there. A column's logical type comes from the first batch: every non-null value of one
  BSON type that maps exactly (integers to `Int`, doubles to `Float`, `bool`, `string` to
  `Text`, dates to `TimestampTz`, `decimal128` to `Decimal`), otherwise `Json`, each value as
  relaxed Extended JSON. `spelaion.native_type` is the BSON type, or `mixed`. A field of a later
  document that has no column, or whose type does not fit its column, goes into `…` as part of
  a JSON object of that document's leftover fields (its typed cell is null). Nothing is lost
  and no column changes type halfway through a result (ADR-005's 2026-11-12 note); the cell
  viewer shows a row as its whole document.
- **Command replies** (Redis): a scalar is one row with a `value` column; a list is one row
  per element; a map or hash is `field`, `value` rows; binary values are `Bytes`, text as
  `redis-cli` prints it.
- **Search hits:** one row per hit, `_index`, `_id`, `_score`, then the `_source` fields as
  documents are; aggregations as `Json`. Through their SQL endpoint, plain rows.
- **Graphs:** Cypher already returns rows of named values; a node, relationship or path is a
  `Json` cell with native type `node`, `relationship` or `path`. Drawing a graph is a feature
  of the app and gets its own item.
- Credit (`stream.credit`) paces these results as it does SQL's: a plugin asks its server for
  the next page (MongoDB's `getMore`, Elasticsearch's search after) only when it has credit.

### 4. The guard and connection types
`check_script` and `check_run` stay the only gate, and the UI still decides nothing:

- **`Destructive`** always asks, as `DROP` does: dropping a database, collection, index or
  keyspace; `deleteMany` or `updateMany` with an empty filter, and `$out` / `$merge` over an
  existing collection; `FLUSHDB`, `FLUSHALL`, `CONFIG SET`, `SCRIPT FLUSH`, `SHUTDOWN`;
  `DELETE /index`, `_delete_by_query` that matches everything;
  CQL's and SQL++'s `DROP` and `TRUNCATE`; Cypher's `DELETE` and `DETACH DELETE`. Each
  language's list is in the core, tested against its engine.
- **`Write`** and **`Unknown`** ask wherever the connection's type confirms data changes
  (Production, ADR-029), and `Unknown` is never treated as a read. Safeguards only add up, so a
  type that confirms every run confirms these too.
- Commands that can stop a server or cost money run only when the user types them, never from
  something the app runs on its own: no `KEYS *`, unbounded `SCAN`, DynamoDB `Scan`,
  Timestream query or `countDocuments` over a whole collection from the explorer or a
  refresh. `KEYS` typed by the user asks on a type that confirms data changes, as a write does.
  A command that runs a script on the server (`EVAL`, `FCALL`, MongoDB's `$function`) is
  `Unknown`: the core cannot see what it does.

### 5. What applies, and what is hidden
The core tells the app which features a session has, from the engine's entry in the core and
the plugin's capabilities; the app hides the rest instead of showing it broken (ADR-005: never
`if engine == mongodb`).

- **Works:** the editor (with a mode for the language and names from the explorer), script runs
  and their error modes, history, the result grid and everything over stored results (sort,
  filters, find, calc panel, copy, charts, the cell viewer), the explorer (databases, schemas
  or keyspaces, objects, their indexes), the connection test, tunnels, proxies and Kubernetes,
  masking (point 7) and the audit of what the app ran.
- **Opening an object** in the explorer opens an editor tab with the engine's `browse_request`
  (`db.orders.find({}).limit(200)`, `SCAN 0 MATCH * COUNT 200`, `GET /orders/_search`) and runs
  it: bounded, and visible as the request it is.
- **Hidden, because the core writes SQL for them:** table data tabs and their filters and
  references, editing by primary key and pending edits, import, streaming exports (ADR-034
  re-runs a read inside the engine's read-only transaction), column statistics, the data search,
  structure forms, object actions, users and privileges, backup and restore, ER diagrams,
  execution plans, manual commit and smart commit (ADR-031 needs the core to know the language's
  transactions). Each comes back for an engine with its own item, written in the core for that
  language. A language shaped like SQL gets the relational ones first: CQL and PartiQL describe
  tables, columns and keys in the common model.

### 6. Read-only mode and agents
A non-SQL engine is offered read-only mode and agents only when, for that engine, the core has:

1. a **read classifier** in its language entry that accepts only requests from a closed allow
   list (MongoDB: `find`, `aggregate` without `$out` or `$merge` anywhere in the pipeline,
   `count`, `distinct`, the list commands; Redis: commands the core's own table marks as reads,
   nothing that runs a script; an HTTP language: `GET` on paths of its list), and refuses
   everything else and whatever it cannot read; and
2. a **barrier the engine enforces**, as ADR-020 asks of each SQL engine: a read-only session
   or transaction the server keeps (Neo4j's read access mode, Cassandra and Elasticsearch
   privileges checked when the session opens, MongoDB's `connectionStatus` showing a user with
   no write action, Redis ACLs with no write category), or operations the engine documents as
   never writing with no user code inside them; plus the time and row limits of ADR-008; and
3. tests that try to get around both, in `tests/agents_engines.rs` and
   `tests/read_only_connection.rs`, against the engine in a container.

Until then: the connection form does not offer "read-only" or agents for that engine; a
connection file with `read_only = true` on it does not open and says the engine has no
read-only mode yet (silently allowing writes is what ADR-053 exists to stop); `spelaiondb mcp`
does not list its connections, whatever `mcp.exposed` says, and the app's MCP page says why;
agents' writes follow reads (ADR-019). **In v0.10 no engine of these families has them.**
Each engine's barrier is an item of its own, decided against what that engine's server does.

### 7. Masking
Column masking (ADR-051) matches result columns by name, so it works on top-level fields; for
documents the core also masks the keys of the `…` column, so a field that arrives late is not
let through. Fields nested inside a `Json` value are not matched, which the documentation says:
masking is not access control.

### 8. The order
One engine first, then the families, as the catalogue's items say: **MongoDB** (the furthest
from rows and columns, so it tests this decision most), then Redis and the Cassandra protocol,
the search engines, the time-series languages and Neo4j. Engines that speak another engine's
protocol (FerretDB, DocumentDB, Valkey, ScyllaDB, OpenSearch) are a kind of connection of that
engine's plugin, checked one by one against a real server, never assumed. What the first
engine teaches about the shape of `execute_request`, `options` and the result rules may change
them before a second plugin depends on them; after that, only additions.

## Consequences
- These engines reach v0.10 with an editor, results, an explorer and the guard, and without
  agents, read-only mode, table data tabs, edits, import or exports to a file. The app says so
  where the feature would be, instead of leaving users to discover it.
- The core grows a parser per request language (MongoDB's shell subset, Redis's command line,
  HTTP requests) and a dialect per language shaped like SQL. They are small and have no network
  dependency, but they are code the core owns and tests against each engine, as ADR-041 wants.
- Users of MongoDB cannot paste arbitrary mongosh scripts (variables, loops, helper functions).
  `db.runCommand` reaches every command, so nothing is out of reach, only the JavaScript.
- A document result's columns depend on its first batch: two runs of the same query over
  changed data can show different columns. The `…` column makes that visible, not lossy.
- A plugin author of someone else's non-SQL plugin (ADR-055) can only offer an engine whose
  language the core already has: without our language entry the core cannot split, guard or
  send requests to it. That is the price of not asking plugins what is a read.
- Protocol 1.1 is the first minor version; the core then speaks two, which ADR-042 already
  expects.

## Alternatives
- **Leave these engines to other people's plugins** (SDB-154): no work here, and no guard, no
  read-only mode and no agents on them for good, with results shaped however each author
  likes. The owner wants them in the product; that rules it out as the main path.
- **Plugins that bring their own language and classification:** the core would be asking the
  program it guards against whether a request is a read (ADR-041's alternatives).
- **Only SQL fronts** (Elasticsearch's SQL, MongoDB's Atlas SQL interface, PartiQL): every
  statement through `sqlparser`, but Redis has none, MongoDB's needs Atlas, and every SQL front
  reaches only part of its engine. Used where it is the engine's own (point 1), not as the only
  way in.
- **Raw command documents only for MongoDB** (`{"find": "users", …}`), no shell syntax: the
  simplest to read, and foreign to every MongoDB user and every example in its documentation.
  The shell subset maps one to one onto command documents, and `db.runCommand` is the raw form.
- **Embedding the engines' own shells** (mongosh, a Gremlin console): everything their users
  know, and nothing the core can classify or guard; the guard would have to ask before every
  run.
- **Sending the typed text and letting the plugin parse it:** one parser fewer in the core, and
  two parsers that must agree for the guard to mean anything.
- **One column per result with the whole document as JSON:** simplest, and no sorting, filters,
  charts or calc panel on fields, which is most of what a grid is for.

## Revisit if
- The first engine (MongoDB) shows the request shape, `options` or the result rules do not fit:
  they change before a second plugin depends on them.
- Users need the JavaScript of a shell (variables, loops): a sandboxed evaluator in the core
  that only produces requests could be weighed in its own ADR.
- An engine's barrier proves impossible (no read-only user, role or session the server keeps):
  that engine stays without read-only mode and agents for good, and this ADR records it.
- Other people's plugins for languages the core does not read are what users ask for most: how
  a plugin could bring a language the core can still check gets its own ADR (ADR-041's revisit).
