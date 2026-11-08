# MariaDB execution-plan fixtures

Unmodified plans captured from MariaDB 11.4.13 (`mariadb:11.4` in `testing/docker/compose.yaml`)
on 2026-11-08 by [`../capture-mysql.sh`](../capture-mysql.sh), with the same synthetic
`setup.sql` (1,000 customers, 120,000 orders) in a uniquely named database that the script
drops afterwards. Each `<name>.sql` runs through the server's `mariadb` client in batch, raw
mode and its single result cell is written unchanged to `<name>.json`. `provenance.json`
records the server version, image and SHA-256 of each file. No application, personal or
production data was read.

| Fixture | What it shows |
| --- | --- |
| `simple.json` | A constant primary-key row, with no cost to tell the flavor by |
| `join.json` | Sort over a temporary table over a nested loop |
| `subquery.json` | Union, materialized derived table, cached dependent subquery |
| `analyze-join.json` | ANALYZE: loops, r_rows × r_filtered, page accesses, sort time |
| `analyze-hash.json` | ANALYZE: block nested loop (BNL) holding most of the time |
| `analyze-never.json` | ANALYZE: every row removed by the attached condition |
| `analyze-subquery.json` | ANALYZE: subquery run 100 times, materialization, `No tables used` |

Metric semantics follow MariaDB's documentation of
[EXPLAIN FORMAT=JSON](https://mariadb.com/kb/en/explain-format-json/) and
[ANALYZE FORMAT=JSON](https://mariadb.com/kb/en/analyze-format-json/).
