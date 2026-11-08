# PostgreSQL execution-plan fixtures

These are unmodified JSON responses captured from PostgreSQL 17.11 in the project's Docker
service on 2026-11-08. `provenance.json` records the server version, image digest and SHA-256
of each file. The coordinator generated them in an exclusively owned temporary database,
using 1,000 synthetic customers and 120,000 synthetic orders, then dropped that database.
No application, personal or production data was read.

Run `setup.sql` in an empty disposable database, followed by each named `.sql` through psql
with `-X -q -A -t -v ON_ERROR_STOP=1`, to reproduce the source queries and session settings.
Timings, buffers and planner choices vary by server; the snapshots are real captured plans,
not exact performance expectations. The parallel capture actually launched workers and the
spill capture actually used disk (validated before recording provenance).

`simple` is estimated only; `join`, `cte`, `parallel` and `spill` include execution metrics.
Parser tests assert meaningful shape and optional metrics rather than fixed timings.

Metric semantics follow PostgreSQL's documentation:
[EXPLAIN](https://www.postgresql.org/docs/17/sql-explain.html) and
[using EXPLAIN](https://www.postgresql.org/docs/17/using-explain.html).
Actual rows and time are averages per loop; buffer counters already include child work.
