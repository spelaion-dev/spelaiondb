# tokio-postgres 0.7.18, patched for SpelaionDB

This is the crates.io release (MIT or Apache-2.0), used through `[patch.crates-io]`.
The client exposes `transaction_status()`, the last PostgreSQL ReadyForQuery status,
returning `None` while requests are outstanding. The connection records the status as it
drains each response, including responses whose receiver was dropped. This lets the driver
avoid issuing a keep-alive within a transaction, including an aborted one. The status is
shared using atomics; no credentials or query content are stored.

When upgrading, reapply the patch or replace it with an upstream API that reports the same
protocol status. Run the PostgreSQL driver and core health tests against a real server,
including the checks for an idle transaction and a failed transaction.
