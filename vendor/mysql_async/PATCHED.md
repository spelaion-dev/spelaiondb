# mysql_async 0.37.1, patched for SpelaionDB

The [mysql_async](https://crates.io/crates/mysql_async) crate (MIT or Apache-2.0, see the
licence files), copied from its crates.io release with one change, used through
`[patch.crates-io]` in the workspace `Cargo.toml`. Only `src/`, the manifest (without its test
targets), the README and the licences are kept.

## The change: `verify-ca` checked the server's name like `verify-full`

With `SslOpts::with_danger_skip_domain_validation(true)` (SpelaionDB's `verify-ca`: the
server's certificate must come from a trusted authority, whatever name it has), the rustls
verifier skipped the name check only when the error's message contained
`"NotValidForName"`. rustls 0.23 reports that error as
`CertificateError::NotValidForNameContext`, whose message reads "certificate not valid for
name …": the check never matched, and connecting by IP address or by a name the certificate
does not list failed.

`DangerousVerifier::verify_server_cert` (in `src/io/tls/rustls_io.rs`, marked "SpelaionDB
patch") now matches `rustls::Error::InvalidCertificate(NotValidForName |
NotValidForNameContext { .. })`. Any other certificate error (an unknown authority, an
expired certificate) still fails.

`crates/spelaion-driver-mysql/tests/mysql.rs` (`tls_modes_encrypt_and_verify`) checks
`verify-ca` by IP address against MySQL and MariaDB, and that a certificate from another
authority is still refused.

To upgrade: take the new release, re-apply the change if upstream has not fixed it (or drop
this folder and the `[patch.crates-io]` entry if it has), and run that test.

## Also: lints allowed (`src/lib.rs`)

A path dependency is not lint-capped like a crates.io one, so `cargo clippy -- -D warnings`
would fail on upstream code. `#![allow(warnings)]` keeps the copy as released.
