# SQL Server visual plan

`sqlserver-spill.png` shows the native execution-plan viewer opening the genuine
SQL Server 2022 `spill.sqlplan` fixture generated from synthetic data. The sort
warning is present in the server's actual Showplan XML; its SQL and provenance are
documented in `crates/spelaion-core/tests/fixtures/plans/sqlserver/README.md`.

The screenshot and the additional estimated, analyzed and ordinary query checks
were produced by `node apps/desktop/e2e/plans-sqlserver.mjs docs/screenshots/sdb-48`
using the built application and an isolated personal and WebView profile.
