# tiberius 0.13.0, patched for SpelunkDB

The [tiberius](https://crates.io/crates/tiberius) crate (MIT or Apache-2.0, see the licence
files), copied from its crates.io release with one change, used through `[patch.crates-io]`
in the workspace `Cargo.toml`. Only `src/`, the manifest, the README and the licences are kept.

## The change: cancelling (`Client::cancel_query`) left the session one response behind

After a TDS Attention, SQL Server 2022 first ends the cancelled response (a DONE with the
error bit) and then acknowledges the Attention in a message of its own (a DONE with
`DONE_ATTN`). `Connection::cancel_request` read only the first message, failed with "Never
got a DONE token acknowledging the Attention signal" and left the acknowledgement on the
wire: the next query read it as its own (empty) response, and every response after that was
the previous query's.

`cancel_request` (in `src/client/connection.rs`, marked "SpelunkDB patch") now:

1. remembers whether the cancelled response had been read to its end;
2. if not, discards the partial packet and drains that message on packet boundaries (a
   stream dropped in the middle of a token cannot be parsed further), returning at once if
   its last token already is the acknowledgement;
3. reads the next message for the acknowledgement.

`crates/spelunk-driver-sqlserver/tests/sqlserver.rs` checks a cancel while waiting and while
rows stream, and that later queries get their own results.

To upgrade: take the new release, re-apply the change if upstream has not fixed it (or drop
this folder and the `[patch.crates-io]` entry if it has), and run those tests.

## Also: lints allowed (`src/lib.rs`)

A path dependency's warnings are not capped as those of crates.io dependencies are, and with
the features SpelunkDB leaves out (Windows authentication) some code is unused: CI's
`-D warnings` would fail on code that is not ours. `#![allow(warnings)]` keeps them quiet.
