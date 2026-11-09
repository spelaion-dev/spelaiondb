# Internal metadata and transaction counts (SDB-95)

These images come from the real Windows Tauri/WebView2 smoke test, using an isolated
temporary profile and the project's Docker PostgreSQL behind its test SSH bastions. The
Production badge tests the application's safeguards; no production database or personal
configuration is used.

`before-metadata-count.png` is the actual smoke failure discovered while validating SDB-17:
the `8f41f56` executable plus the then-uncommitted SDB-17 work (preserved in stash
`08628f63353b4794de4bef5ad1a971d503b00f16`). One successful `CREATE TEMP TABLE` shows
`TX open · 2` because the completion catalog's subsequent schema read is counted too.

`after-metadata-count.png` is the same smart-commit step from this change's full native
smoke test, built on `23275c705306f13872afb3695b7b39dadb129c5b`. It shows `TX open · 1`.
The change is in the core's statement accounting; the badge and its assertion are unchanged.

The regression tests exercise actual schema reads on all supported engines, PostgreSQL's
smart-commit temporary-table case, ordinary generated actions that still count, and shared
session serialization, partial-result cleanup and cancellation. Native UI verification is
Windows-only; macOS/Linux UI and an actual screen reader are not exercised here.
