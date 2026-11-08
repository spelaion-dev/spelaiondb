# History and saved-query library (SDB-17)

These are screenshots of the real Windows Tauri/WebView2 application, using an isolated
temporary personal profile. `apps/desktop/e2e/library.mjs` seeds 10,000 local history records
and three saved queries. They represent safe synthetic executions; the test does **not** run
10,000 database queries. No personal configuration, credentials or production data is used.

The `before-*` images use the saved executable from the source tree merged as `dev` commit
`91b899d97946922f0d1aae526b5fbf7bef751274` (SHA-256
`B09139B609CCD89C13F9662919FFD1E23C433FC9A5A82789FF25A1B52BEDEDEF`).
The `after-*` images use this change's native build on top of `6b9b74ac0b6ff2119d1bc4f3bafb3000a5e7602e`,
including the newer selection-statistics and connection-organization work, which does not change these panels.
Both sets use the same deterministic
history and query text, light/dark themes, and 1400×900 / 900×600 viewports.

| Panel / theme | Viewport | Before | After |
| --- | --- | --- | --- |
| History / light | 1400×900 | [Before](before-history-light-1400.png) | [After](after-history-light-1400.png) |
| History / light | 900×600 | [Before](before-history-light-900.png) | [After](after-history-light-900.png) |
| History / dark | 1400×900 | [Before](before-history-dark-1400.png) | [After](after-history-dark-1400.png) |
| History / dark | 900×600 | [Before](before-history-dark-900.png) | [After](after-history-dark-900.png) |
| Saved queries / light | 1400×900 | [Before](before-saved-light-1400.png) | [After](after-saved-light-1400.png) |
| Saved queries / light | 900×600 | [Before](before-saved-light-900.png) | [After](after-saved-light-900.png) |
| Saved queries / dark | 1400×900 | [Before](before-saved-dark-1400.png) | [After](after-saved-dark-1400.png) |
| Saved queries / dark | 900×600 | [Before](before-saved-dark-900.png) | [After](after-saved-dark-900.png) |

The history now has status and database filters, an optional exclusive date range and pinned
filter, date headings, persisted pins, duration/row/status details, and SQL export of the
matching history across every page. History and saved queries render one page of at most 50
entries, with Previous/Next controls. The saved-query screenshots show folders created and a
query moved using the real UI. Opening a query creates a disconnected editor tab without
executing it.

[Empty pins](empty-pins.png) shows the empty filtered state and disabled export. [Load error](load-error-dark-900.png)
shows a real core validation error and Retry: the native test temporarily modifies only the
history command's date arguments with a CDP function breakpoint, then retries without the
fault. It does not replace the UI's results with fabricated responses.

The regression also exercises history/saved-query paging, SQL text search, status/database
combinations, pin/unpin, a filtered SQL file, moving between folders, and modal keyboard focus,
Tab wrapping, Escape, and safe delete cancellation. Additional saved queries and an empty
error-message marker are seeded only after the main screenshots to exercise paging beyond
the first page and to verify that an empty error message still has Error status.

Storage tests use the actual production SQL builder with 10,000 history rows and
`EXPLAIN QUERY PLAN`, checking the chosen status/database/pinned/time indexes and FTS index,
including anchored later pages. Migration tests preserve existing SQL, preferences and
history opt-out. Export reads bounded pages in one SQLite snapshot and atomically replaces
the destination only on success; cancellation preserves an existing destination.

Database context comes from SQL-free source-actor snapshots before each statement on
MySQL/MariaDB, SQL Server and DuckDB. The actors seed only known connection context and track
successful unambiguous `USE`; DuckDB obtains its initial catalog during connection
initialization, before user SQL. Existing explicit `current_database` reads can refresh the
cache. History never invokes those queries: real MySQL/MariaDB tests reproduce and prevent
the extra SELECT changing `ROW_COUNT()`. SQL Server compares with its measured unwrapped
Session/TDS baseline. Other engines use the source session's immutable configured database or file.

Old entries, failed snapshots and uncertain context remain unknown. Opaque execution,
compound context changes, SQL Server parse/plan-only modes, DuckDB one-part `USE` and
schema/search-path changes invalidate certainty until an authoritative explicit read.
The bounded observer also declines SQL over 1 MiB and unresolved MySQL backslash quoting;
it never authorizes or rewrites SQL. Real tests exercise quoted/failed `USE`, prepared or
bare procedure execution, SQL Server execution modes, and DuckDB `SET schema`/`RESET search_path`.
SQL Server plan output is not accepted as a database-name response.

The tests also cover a snapshot failure that leaves execution intact and cancellation
while snapshot retrieval is paused that executes and records no statement. Cancellation
stops requested runs before their first poll, while waiting for the history actor or the
session's control lock. A fresh run requested after that cancellation remains runnable.

Known SQL credential patterns and URL passwords use the core's existing audit redaction
heuristic before recording, displaying and exporting, including legacy rows on read. This
cannot identify arbitrary secrets in SQL literals; the existing per-connection history
opt-out remains available and is preserved. Ordinary SQL retains its exact text.

Validation on base `6b9b74a`: workspace formatting and Clippy passed; 687 Rust tests passed
with two optional tests ignored, using all seven real Docker endpoints. The 88 frontend
tests, strict TypeScript/build, focused native library regression and clean full native
smoke passed. An earlier Auto-commit smoke timeout is tracked separately as SDB-98; its
cause remains unconfirmed. The observational and final clean runs passed that transition.

Windows native checks are performed here. macOS/Linux UI, an actual screen reader,
Kubernetes and an external SSH agent are not exercised locally.
