# The plugin protocol

**Version 1.0. Experimental until SpelaionDB 1.0: it may change in any 0.x release.**

An engine plugin is a program the core starts and talks to over the program's standard input
and output (ADR-041). It is the wire to one or more engines: it connects, runs statements,
cancels, introspects and loads rows. It is never asked what may run: splitting, classifying
and guarding statements, secrets, tunnels and the audit are the core's.

This file is the contract. `spelaion-plugin-protocol` is its types, `spelaion-plugin-sdk` is
the plugin's side (a Rust plugin implements `spelaion-driver-api`'s `Driver` and `Connection`
and calls `spelaion_plugin_sdk::run`), and `spelaion-plugin-host` is the core's side. A plugin
in another language implements what is written here.

## The process
- The core starts the plugin with **no arguments and no environment of its own** that carry
  anything about a connection. Everything arrives in messages on standard input.
- One process serves every session of its engines in that program (the app, or `spelaiondb
  mcp`). It is started the first time a connection of one of its engines opens.
- The plugin **exits when its standard input closes**. The core also stops it when the
  program ends.
- **Standard output is the protocol and nothing else.** Anything a plugin wants to say goes
  to standard error, one line at a time: the core logs it after taking out the secrets it
  gave that plugin.
- A plugin that exits, or whose output stops being the protocol, loses every session it had:
  the core marks them lost as it does a dropped connection (ADR-028). Nothing is re-run. The
  next connection starts the plugin again.

## Frames
Both directions carry frames, one after another:

| Bytes | Meaning |
|---|---|
| 4 | Length of what follows this field, unsigned, little-endian |
| 1 | Kind: `1` a message, `2` data |
| … | The payload |

A frame is at most 64 MiB; a longer one ends the conversation.

- **Message** (`1`): one JSON-RPC 2.0 object in UTF-8. Requests carry `id`, `method` and
  `params`; responses carry `id` and either `result` or `error`; notifications carry no `id`.
  Batches (JSON arrays) are not used.
- **Data** (`2`): 8 bytes of stream id (unsigned, little-endian), 1 byte of flags, then bytes
  of an Arrow IPC stream. Flag `1` means these bytes start a new IPC stream on that id (a
  schema follows). **Rows never travel as JSON.**

Frames of different sessions and streams are interleaved. The order of the frames of one
stream is the order of its events.

## Versions
The protocol has a version, `major.minor`. Within a major version changes only add: new
optional methods, notifications, fields and capabilities. A receiver ignores fields it does
not know and answers an unknown method with JSON-RPC's `-32601`. A change that breaks is a new
major version.

`initialize` is the first request. The core says every version it speaks; the plugin picks
one or refuses:

```json
{"jsonrpc":"2.0","id":1,"method":"initialize",
 "params":{"protocols":[{"major":1,"minor":0}],"host":{"name":"spelaiondb","version":"0.10.0"}}}

{"jsonrpc":"2.0","id":1,
 "result":{"protocol":{"major":1,"minor":0},
           "plugin":{"name":"spelaiondb-plugin-duckdb","version":"0.1.0"},
           "engines":["duckdb"],"credit":8}}
```

The plugin answers with the highest version of a major it speaks that is not above its own
(the core offered 1.3 and the plugin speaks 1.1: it answers 1.1, and neither uses what 1.2
added). A plugin that speaks none of them answers with error `-32001` and, in
`data.protocols`, the versions it does speak. The core refuses a plugin whose version it does not speak and says
which it expected. `engines` are the names the configuration uses (`engine = "duckdb"`); a
plugin may serve several (`mysql` and `mariadb`). `credit` is explained under "Results".

## Errors
A request that fails answers with an `error` whose `code` is `-32000` and whose `data` is the
driver's error:

| `data.kind` | Other fields | Meaning |
|---|---|---|
| `connection` | `message` | The connection could not be made or was lost |
| `database` | `message`, `code`, `detail`, `hint`, `position` | The server refused the statement |
| `cancelled` | | The statement was cancelled |
| `decode` | `type_name`, `message` | A value could not be converted |
| `other` | `message` | Anything else |

Messages never contain secrets. The core still takes out of every error, notice and log line
the secrets it gave that plugin.

## Sessions
`connect` opens a session. The core chooses its id (`session`, unique in that process):

```json
{"jsonrpc":"2.0","id":2,"method":"connect","params":{"session":1,"engine":"postgres",
 "host":"db.internal","port":5432,"user":"app","password":"…","database":"shop",
 "application_name":"SpelaionDB","tls":"verify-full","ca_certificate":null,
 "client_certificate":null,"client_key":null,"read_only":false,
 "connect_addr":"127.0.0.1:50211","connect_timeout_ms":15000}}

{"jsonrpc":"2.0","id":2,"result":{"capabilities":{"transactions":true,"cancel":true,
 "explain":true,"editable_results":true,"schemas":true,"insert_rows":true,
 "bound_params":true}}}
```

- **The password travels here and nowhere else.** A plugin keeps it in memory for as long as
  it needs it and never writes it anywhere.
- The core opens tunnels, proxies and forwards. `connect_addr`, when present, is the address
  to connect to (the local end of a tunnel); `host` is still the name to verify in TLS, which
  the plugin does itself. Certificate fields are paths to PEM files.
- `tls` is `disable`, `prefer`, `require`, `verify-ca` or `verify-full` (libpq's `sslmode`).
- A plugin never reads configuration files, the keychain or the local store, and gets no
  tunnel: only what `connect` says.

`close` (`{"session":1}`) ends a session, also one whose `connect` has not answered yet (the
core stopped waiting, or the user stopped the login): the plugin stops connecting there and
then and closes whatever that attempt had open. It does not wait for the server. `session.closed` (a
notification from the plugin,
`{"session":1}`) says the plugin knows the connection is closed (the server ended it, the
network dropped it): the plugin sends it as soon as it knows, before the response of the
request that found out.

Every other request names its `session`. Requests of one session may arrive while another of
the same session is still running (a `cancel` during an `execute`): a plugin answers each
when it is done, in any order.

## Creating a database that is a file
`create_database` (`{"engine":"sqlite","database":"/home/me/new.db"}`, answered with `{}`) is
for engines whose databases are files: it creates an empty one at that path, which is what
`connect`'s `database` will then be. It names no session. An existing file is never touched:
the plugin answers with an error instead. An engine whose databases are not files answers with
an `other` error.

## Running a statement
`execute` runs **one** statement. The core has already split the script.

```json
{"jsonrpc":"2.0","id":7,"method":"execute","params":{"session":1,"stream":4,
 "sql":"select * from t","batch_size":10000,"origins":false,"run":{"kind":"statement"}}}
```

`run` is `{"kind":"statement"}`, `{"kind":"bound","params":["a","b"]}` (one read with text
values bound to the engine's placeholders, never written into the SQL) or
`{"kind":"plan","mode":"estimated"}` / `{"kind":"plan","mode":"analyze","query_result":true}`
(the engine's own plan request). `stream` is chosen by the core and unique in the process.

The response says only that the statement started (`{}`) or why it could not. The results
follow on the stream, in this order, exactly as `ExecEvent` (ADR-005):

1. For each result set: a data frame with flag `1` that carries the Arrow schema, then data
   frames with its record batches. Each field carries the metadata `spelaion.logical`,
   `spelaion.native_type` and, where known, `spelaion.native_id`, `spelaion.origin_table` and
   `spelaion.origin_column`. A statement that returns several result sets repeats this.
2. `stream.notice` notifications (`{"stream":4,"severity":"NOTICE","message":"…"}`), wherever
   the server sent them.
3. `stream.done` (`{"stream":4,"rows_affected":3}`, `null` when there is no count) when the
   statement finished, or `stream.error` (`{"stream":4,"error":{…}}`, the same object as an
   error's `data`) when it failed, even after it returned rows.
4. `stream.end` (`{"stream":4}`), always and last: nothing follows it on that stream, and its
   id is free.

The bytes of one result set are one Arrow IPC stream (a schema message, then record batches,
with dictionaries if the plugin uses them). They may be cut into data frames anywhere: a
batch larger than a frame simply continues in the next.

### Results are sent as the core asks for them
A plugin may have sent, and not yet been given credit back for, at most `credit` record
batches per stream (the number it gave in `initialize`). The core gives credit back as it
consumes them: `stream.credit` (`{"stream":4,"batches":1}`). A plugin with no credit left
waits before reading more from the server. Schemas, notices and the end of the stream need no
credit. This is what keeps a large result from filling the core's memory.

`stream.drop` (`{"stream":4}`) says the core will read no more of that stream: the plugin
stops it as if its consumer had gone (it does not cancel anything else on the session).

### Cancelling
`cancel` (`{"session":1}`) is a request: it interrupts what that session is running, as the
engine allows (PostgreSQL's cancel request, a TDS attention, `KILL QUERY` from a second
connection). It can arrive while a statement runs and must be obeyed then. The interrupted
statement ends its stream with a `cancelled` error.

## Loading rows
`insert_rows` loads rows into a table (ADR-038). The rows arrive first, as data frames on a
stream the core chooses: one Arrow IPC stream whose columns are nullable `LargeUtf8`, one per
target column, in order. Then the request:

```json
{"jsonrpc":"2.0","id":9,"method":"insert_rows","params":{"session":1,"stream":12,
 "target":{"schema":"public","table":"t","columns":["a","b"]}}}

{"jsonrpc":"2.0","id":9,"result":{"rows":10000}}
```

Every value is bound or goes through the engine's bulk path: never written into SQL text.

## The other requests
Each takes `session` plus what is listed and returns what `Connection`'s method of the same
name returns, as JSON with the field names of `spelaion-driver-api`.

| Method | Parameters | Result |
|---|---|---|
| `server_version` | | `{"version": "…"}` |
| `keep_alive` | | `{"state": "alive" \| "in-transaction"}` |
| `transaction_state` | | `{"state": "idle" \| "open" \| "failed"}` |
| `current_database` | | `{"database": "…" \| null}` |
| `database_context` | | `{"database": "…" \| null}` |
| `select_database` | `database` | `{}` |
| `list_databases` | | `{"databases": ["…"]}` |
| `list_schemas` | `filter`? | `{"schemas": [SchemaInfo]}` |
| `list_objects` | `schema`, `filter`? | `{"objects": [ObjectInfo]}` |
| `describe_table` | `schema`, `name` | `{"table": TableInfo}` |
| `list_referencing_keys` | `schema`, `name` | `{"keys": [ReferencingKey]}` |
| `list_columns` | `schema` | `{"columns": [SchemaColumn]}` |
| `list_schema_objects` | `schema`, `filter`? | `{"objects": [SchemaObject]}` |
| `list_server_objects` | | `{"objects": [ServerObject]}` |
| `object_definition` | `object` | `{"sql": "…"}` |
| `relation_definition` | `schema`, `name` | `{"sql": "…"}` |
| `procedure_call` | `object` | `{"sql": "…"}` |

`filter` is a `NameFilter` (`{"include": ["…"], "exclude": ["…"]}`); with it the result is
narrowed as `list_schemas_matching`, `list_objects_matching` and
`list_schema_objects_matching` do. A plugin that does not support a request answers with the
error its driver gives (`other`), never with silence.

## What a plugin must never do
- Write anything but frames to standard output.
- Read the password from anywhere but `connect`, or write it anywhere.
- Decide whether a statement is a read: it runs what it is sent.
- Re-run a statement after a connection is lost.
- Outlive its standard input.
