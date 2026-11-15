# Flight SQL integration investigation

Status: **not implemented**, investigated for SDB-175 on 2026-11-15, against `dev`
at `770d325`. This is an implementation plan and a record of prerequisites, not an ADR
or a claim that a Flight SQL connection works. No plugin, pin, connection kind or test
server is added by this investigation.

## What can be shared

One external plugin can carry the Flight SQL transport for generic Flight SQL, Dremio and
GizmoSQL connections, as SDB-175 requests. It belongs in a repository of its own, created
from the plugin template (ADR-041, ADR-042), with Arrow Rust on the SDK's Arrow major
version, currently 60. A server's Arrow library release is not a Rust crate version that
the core must adopt: the plugin decodes its wire data into its own Arrow version.

The [Flight SQL specification](https://arrow.apache.org/docs/format/FlightSql.html)
defines metadata commands for catalogs, schemas, tables, table types, primary keys and
imported/exported keys. `GetFlightInfo` supplies tickets fetched with `DoGet`. Queries use
`CommandStatementQuery`; statements without results use `CommandStatementUpdate` through
`DoPut`. `GetSqlInfo` reports server and SQL feature information, not a complete parser.

These facts support sharing the wire implementation. They do not establish a shared SQL
language or an engine-enforced read barrier. ADR-020 and ADR-041 therefore still exclude
agents and read-only mode on the generic connection. A named engine needs its own evidence
before either is offered, even when it shares a plugin with another engine.

## The prerequisite in the core

The present core cannot open an engine with no dialect. In
[`open.rs`](../crates/spelaion-core/src/open.rs), `open_connection` requires
`dialect_of(resolved.engine)` before connecting. A `Session` holds a `Dialect`, and
[`guard.rs`](../crates/spelaion-core/src/guard.rs),
[`params.rs`](../crates/spelaion-core/src/params.rs) and the generated SQL paths depend on
its splitting, code ranges and quoting. Merely registering a plugin or adding an `Engine`
variant does not provide a usable generic connection.

The generic mode needs a decision before implementation (tracked in SDB-340): how explicit user submissions
with an unknown SQL language are executed and confirmed without pretending the core knows
their syntax. Two possible scopes are:

- Require a core dialect selected by the user. This reuses the normal session, but does
  not satisfy SDB-175's generic connection without a dialect, and a chosen dialect still
  grants neither read-only mode nor agents without the engine's barrier tests.
- Add a core-owned opaque submission mode. A selection or the whole editor text becomes
  one explicitly confirmed submission, never split or rewritten. It has no cursor
  statement detection, named parameter expansion, automatic session setup, generated SQL,
  table data tabs, editing, streaming exports or transaction controls. Results already
  stored can use the ordinary grid, masking and copies. Metadata browsing need not generate
  SQL. Whether to offer this mode, and its exact confirmations, belongs in an ADR; this
  investigation does not introduce it through a placeholder `Dialect` implementation.

The second option must distinguish a request returning rows from an update request without
classifying unknown SQL. It must never try one RPC and then retry the text as the other:
an error does not prove the first request changed nothing (ADR-028). Even a submission
returning rows is not necessarily a read. The Flight SQL specification also warns that a
prepared statement's initial result schema is not necessarily its execution schema, so an
initial schema alone is not a general execution-mode discriminator.

## Results need normalization in the plugin

"Already Arrow" does not mean every batch can pass through unchanged. The current contract
in [`value.rs`](../crates/spelaion-driver-api/src/value.rs) gives each logical type one
physical representation, and
[`column.rs`](../crates/spelaion-driver-api/src/column.rs) requires `spelaion.logical` and
`spelaion.native_type` metadata. `Value::from_array` downcasts to those exact arrays.

| Server array | Contract requirement |
|---|---|
| Boolean, Int64, Float64, Binary, Utf8, Date32 | Reuse buffers when metadata and semantics fit. |
| Smaller signed integers and floats | Widen losslessly to Int64 or Float64. |
| UInt64, Decimal128/256 | Exact decimal text; never a lossy signed or floating cast. |
| Time and timestamp arrays | The contract uses microseconds. Preserve sub-microsecond precision as text with the native type, following ADR-005; never truncate it silently. |
| Large/view/dictionary encodings | Normalize to the contract's physical array type. |
| Lists, structs, maps, unions and extensions | Define and test an exact text representation or fail explicitly for the unsupported type; never silently replace it with NULL. |

The conversion is chosen from schema metadata before rows arrive, and remains the same for
every batch (ADR-005). Retain native types and reported origins for masking (ADR-052).
Unknown extension semantics must not be inferred from the storage type alone. Empty and
all-NULL results require the same schema as populated results. None of this requires
expanding the core's accepted Arrow physical types to whatever one server emits.

## Routing, sessions and capabilities

The [Flight RPC specification](https://arrow.apache.org/docs/format/Flight.html) allows
multiple result endpoints and alternative locations for one endpoint. Its `ordered` flag
describes whether endpoint order matters. A plugin must consume each endpoint once, not
each location as a separate partition, and retain endpoint order when required.

ADR-041 requires every database socket to use the core's route (`connect_addr`) while TLS
verifies the original host. A Flight location must not turn a tunneled query into a direct
connection, send its bearer token to another authority, or trigger an environment proxy.
The first implementation should explicitly reject locations it cannot serve on the existing
route, before contacting them. Multi-host routing can be added only with core-owned routes
and tests. Test alternate locations with an instrumented server and assert that no socket
or credential reaches the unapproved destination.

Keep authentication, session cookies/options and prepared-statement handles isolated per
connection, within the shared plugin process. Close what that session owns and mark a lost
session closed; never recreate it and repeat a submission. Server-issued tokens must also
stay out of plugin errors and logs: the host's knowledge of the original password does not
automatically cover a different token. Cancellation during both `GetFlightInfo` and `DoGet`,
bounded queues and SDK credit need tests before declaring cancel supported. Neither Flight
SQL's transaction actions nor its server read-only information establishes ADR-020's
per-session read barrier by itself.

## Named engines and catalog coverage

| Entry | Evidence and remaining check |
|---|---|
| Generic Flight SQL | No common SQL dialect or barrier. Resolve the submission-mode prerequisite above. |
| Dremio | Its [Flight SQL documentation](https://docs.dremio.com/current/developer/arrow-flight-sql/) lists schema, table, table-type and SQL-info metadata. Do not assume every standard metadata method exists. Test optional-method failures and nullable catalog names. Its [client documentation](https://docs.dremio.com/current/client-applications/drivers/arrow-flight-sql-jdbc-driver/) gives port 32010 and distinguishes password/PAT from bearer-token authentication. |
| GizmoSQL | The [upstream project](https://github.com/gizmodata/gizmosql) supports DuckDB and SQLite backends and documents port 31337. Its name therefore does not select DuckDB's dialect or barrier. Test each offered backend explicitly. |

Missing optional catalog methods must be reported as unsupported, rather than interpreted
as proof that a table has no keys. No editing is enabled on inferred keys. Preserve the
difference between an absent catalog/schema filter and an explicitly empty one. Relation
metadata must continue using the driver's common model; routines and DDL definitions have
no universal Flight SQL equivalent to assume.

## Implementation and validation sequence

1. Settle the generic submission mode in an ADR, with the effects on confirmations, result
   versus update requests, history, placeholders and unavailable generated SQL features.
2. Implement that core prerequisite, its generated frontend types and tests that assert
   rejected operations never reach a made-up plugin. Keep the generic connection excluded
   from agents and refuse a configuration asking for read-only mode.
3. Implement the external plugin using the tagged SDK and shared workflows. Add schema
   normalization tests and an instrumented Flight SQL server for endpoints, tokens, credit,
   cancellation, session isolation and a connection lost after a statement is accepted.
4. Add a pinned container of a real engine and run its type zoo and metadata tests. GizmoSQL
   is a candidate, but verify an actual image tag and digest: the repository's release tag
   must not be guessed to be its Docker tag. Add the fixture with the tests that consume it.
5. Test Dremio separately. Then add the validated connection kinds, form, dialects where
   known, development overrides, and core tests through the plugin, including an SSH tunnel
   and TLS verification against the original host.
6. Only after the plugin's platform builds and conformance tests pass, publish its archives
   and submit their real hashes as a pin. A working GizmoSQL test is not Dremio coverage.

No database connection, type zoo, TLS handshake, tunnel or cancellation was tested in this
investigation. SDB-175 remains incomplete until those implementation gates pass. The
blocker is the unresolved generic execution model, not a missing read-only classifier that
could safely be filled in with generic SQL.
