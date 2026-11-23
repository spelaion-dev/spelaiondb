# ADR-067: Data search and append imports on GreptimeDB

- **Status:** Proposed (SDB-355)
- **Date:** 2026-11-23
- **Would supersede:** ADR-043, point 4, for a verified GreptimeDB read credential, and
  ADR-038, point 13, for an explicit GreptimeDB append import.

## Context

GreptimeDB 1.3.1 speaks MySQL's protocol. The pinned MySQL plugin serves browsing, table pages
and cancellation (SDB-344), and advertises neither bound reads nor row imports on GreptimeDB.
The existing bound-read path sets a MySQL user variable and reads it in a text-protocol query.
The existing import path sends every parameter as text.

On a disposable GreptimeDB 1.3.1 server, the official MySQL connector's prepared cursor could
bind a string in `LOCATE(?, label)` and bind a double to a `DOUBLE` column. A string parameter
for that column failed with error 1210. The user-variable lookup failed at planning with
error 1815. A direct prepared path therefore looks feasible for these types. GreptimeDB also
documents typed prepared inserts. [GreptimeDB INSERT reference](https://docs.greptime.com/reference/sql/insert/)

Binding solves neither the read barrier nor rollback. The core integration test
`greptime_read_only_transaction_does_not_stop_writes` passed against that version with the
shipped MySQL plugin 0.1.6. The server accepted a write after `START TRANSACTION READ ONLY`,
and the row remained after rollback.

GreptimeDB's static provider can enforce read-only credentials. On a second disposable
instance, an `ro` account successfully read an existing row and got error 1045 on an insert.
For both the `ro` and `rw` accounts, `SHOW GRANTS` was unsupported, the user and table privilege
views returned zero rows, and `CURRENT_USER()` reported the account name only. Those responses
cannot establish a connection's effective permissions.
[GreptimeDB static user provider](https://docs.greptime.com/user-guide/deployments-administration/authentication/static/)

## Proposed decision

1. **Data search requires a verified server-side read barrier.** A GreptimeDB search may use a
   separate read credential in its own session if the server provides a non-mutating check
   that can establish the effective permissions of the authenticated account. The core owns
   the decision. Missing or ambiguous evidence refuses the search before its first table.
   The SQL classifier, row and time limits, masking and cancellation still apply. The scope
   of this exception is the app's generated search statements.
2. **GreptimeDB uses direct prepared statements.** The core selects placeholders and writes
   the search for the recognised server family. The plugin sends the values apart from the
   SQL and decodes the binary result through the driver contract. Numeric comparisons must
   respect GreptimeDB's types and precision. For import, the plugin uses the destination
   schema to convert the text or NULL from the core to typed parameters and reports
   unsupported or out-of-range values with their row and column.
3. **An append import is a distinct, non-atomic operation.** It may target an existing
   append-only table, with no table creation, replacement or schema changes. The preview
   explains that acknowledged rows can remain after a failure or cancellation, and the
   core requires explicit confirmation in addition to the existing write safeguards.
   Prevalidate each batch before sending it. The result says how many rows the server
   acknowledged, which input range failed and whether the last batch's outcome is unknown.
   Stop on the first failure or cancellation. An ambiguous request is never retried.

## Verification required before enabling either path

- Demonstrate that the permissions check distinguishes read-only and writable accounts on
  the same server without issuing a write probe. Cover missing, stale and ambiguous evidence,
  and verify the read-only credential with a real write-denial fixture. The permission
  check remains an open prerequisite on GreptimeDB 1.3.1.
- Add MySQL plugin tests in `tests/greptimedb.rs` against the real engine for bound reads,
  exact numeric comparisons, typed inserts, NULL and cancellation. Include legitimate
  values with quotes and delimiters.
- Add core integration tests in `crates/spelaion-core/tests/mysql_family.rs`. Search must
  refuse a writable or unverified credential. Import tests must check stored rows after a
  confirmed batch, a rejected batch, cancellation and a lost response. Only mark rows as
  confirmed when the driver has an acknowledgement.
- Release the plugin and move its pin only after the relevant core and plugin tests pass
  together, as ADR-042 requires.

## Alternatives

- **Keep search and imports unavailable.** This is the current behavior and remains the
  default until the relevant gate is met.
- **Use the MySQL transaction syntax on GreptimeDB.** The server accepts it without enforcing
  either guarantee, which would let a writable session search and misstate rows after import.
- **Ask the user to attest that a credential is read-only.** A file or a UI choice cannot prove
  the effective privileges of the account the server authenticated.
