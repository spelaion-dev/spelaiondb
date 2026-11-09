# MySQL execution-plan fixtures

Unmodified plans captured from MySQL 8.4.13 (`mysql:8.4` in `testing/docker/compose.yaml`) on
2026-11-08 by [`../capture-mysql.sh`](../capture-mysql.sh). The script creates its own
uniquely named database on the Docker server, loads the synthetic data of `setup.sql` (1,000
customers, 120,000 orders), runs each `<name>.sql` through the server's `mysql` client in
batch, raw mode, writes the single result cell unchanged to `<name>.json` (EXPLAIN FORMAT=JSON)
or `<name>.txt` (EXPLAIN ANALYZE FORMAT=TREE), and drops the database. `provenance.json`
records the server version, image and SHA-256 of each file. No application, personal or
production data was read.

| Fixture | What it shows |
| --- | --- |
| `simple.json` | A constant primary-key row |
| `join.json` | Format version 1: nested loop, grouping with a temporary table, filesort |
| `subquery.json` | Union, materialized derived table, dependent select-list subquery |
| `join-v2.json` | The same join with `explain_json_format_version = 2` (an operator tree) |
| `analyze-join.txt` | ANALYZE: 30,000 loops of a primary-key lookup |
| `analyze-hash.txt` | ANALYZE: hash join, LIMIT, sort, scientific-notation costs |
| `analyze-never.txt` | ANALYZE: a lookup that never executed |
| `analyze-subquery.txt` | ANALYZE: append, materialize, dependent subquery |

Re-run `bash crates/spelaion-core/tests/fixtures/plans/capture-mysql.sh` with the containers up
to refresh them (timings and estimates vary; the tests check shapes and engine figures, not
fixed timings). Metric semantics follow MySQL's documentation of
[EXPLAIN output](https://dev.mysql.com/doc/refman/8.4/en/explain-output.html) and
[EXPLAIN ANALYZE](https://dev.mysql.com/doc/refman/8.4/en/explain.html#explain-analyze).
