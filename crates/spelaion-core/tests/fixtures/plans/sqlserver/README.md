# SQL Server Showplan fixtures

Captured from SQL Server 2022 Developer Edition 16.0.4305.5 (CU27 GDR), Linux x64,
using the Docker test service. The SQL files here are the exact queries and data setup.
`setup.sql` runs in a newly created disposable database; all data are synthetic.
Each estimate was captured with `SET SHOWPLAN_XML ON`, and `parallel` and `spill`
were captured with `SET STATISTICS XML ON` (ordinary query rows discarded).
Each SET was sent in a separate batch, then turned off before disconnecting.

`parallel.sqlplan` contains four workers plus the coordinator. `spill.sqlplan`
contains a real sort warning, level 2, after limiting the grant to 0.01 percent
and sorting 300,000 synthetic rows. No warning or runtime counter was fabricated.
SQL Server includes the disposable database name in its XML.

With the Docker test service running, `./generate.ps1` from this directory regenerates
the fixtures. It creates a uniquely named database and drops only that database in `finally`.
