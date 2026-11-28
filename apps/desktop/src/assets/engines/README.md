Engine marks come only from sources whose licence allows them: [Simple Icons](https://simpleicons.org/)
first, [Devicon](https://devicon.dev/) otherwise, never a vendor's site. Each file below is
byte for byte the one in the named package (`npm pack <package>@<version>`, then the path
inside it). Which engine shows which file, and the engines that have none and what they show
instead, is `marks.json` (SDB-363); `scripts/engine-marks.test.mjs` checks that every engine of
the core's catalogue is in it and that every file is listed here. Brand names and marks belong
to their respective owners; the Apache marks follow the [ASF's trademark
policy](https://www.apache.org/foundation/marks).

| File | Engine | Source | Licence |
|---|---|---|---|
| `postgresql-original.svg` | PostgreSQL | devicon@2.18.0 `icons/postgresql/postgresql-original.svg` | MIT (Devicon, notice below) |
| `mysql-original.svg` | MySQL | devicon@2.18.0 `icons/mysql/mysql-original.svg` | MIT (Devicon) |
| `microsoftsqlserver.svg` | SQL Server | devicon@2.18.0 `icons/microsoftsqlserver/microsoftsqlserver-original.svg` | MIT (Devicon) |
| `oracle.svg` | Oracle | devicon@2.18.0 `icons/oracle/oracle-original.svg` | MIT (Devicon) |
| `yugabytedb-plain.svg` | YugabyteDB | devicon@2.18.0 `icons/yugabytedb/yugabytedb-plain.svg` | MIT (Devicon) |
| `sqlite.svg` | SQLite | simple-icons@16.36.0 `icons/sqlite.svg` | CC0-1.0 |
| `mariadb.svg` | MariaDB | simple-icons@16.36.0 `icons/mariadb.svg` | CC0-1.0 |
| `duckdb.svg` | DuckDB | simple-icons@16.36.0 `icons/duckdb.svg` | CC0-1.0 |
| `h2database.svg` | H2 | simple-icons@16.36.0 `icons/h2database.svg` | CC0-1.0 |
| `cockroachlabs.svg` | CockroachDB | simple-icons@16.36.0 `icons/cockroachlabs.svg` | CC0-1.0 |
| `clickhouse.svg` | ClickHouse | simple-icons@16.36.0 `icons/clickhouse.svg` | CC0-1.0 |
| `snowflake.svg` | Snowflake | simple-icons@16.36.0 `icons/snowflake.svg` | CC0-1.0 |
| `googlebigquery.svg` | BigQuery | simple-icons@16.36.0 `icons/googlebigquery.svg` | CC0-1.0 |
| `mongodb.svg` | MongoDB | simple-icons@16.36.0 `icons/mongodb.svg` | CC0-1.0 |
| `redis.svg` | Redis | simple-icons@16.36.0 `icons/redis.svg` | CC0-1.0 |
| `apachecassandra.svg` | Cassandra | simple-icons@16.36.0 `icons/apachecassandra.svg` | Apache-2.0 (as Simple Icons lists it) |
| `turso.svg` | libSQL / Turso | simple-icons@16.36.0 `icons/turso.svg` | CC0-1.0 |
| `tidb.svg` | TiDB | simple-icons@16.36.0 `icons/tidb.svg` | CC0-1.0 |
| `singlestore.svg` | SingleStore | simple-icons@16.36.0 `icons/singlestore.svg` | CC0-1.0 |
| `greptimedb.svg` | GreptimeDB | simple-icons@16.36.0 `icons/greptimedb.svg` | CC0-1.0 |
| `timescale.svg` | TimescaleDB | simple-icons@16.36.0 `icons/timescale.svg` | CC0-1.0 |
| `cratedb.svg` | CrateDB | simple-icons@16.36.0 `icons/cratedb.svg` | CC0-1.0 |
| `trino.svg` | Trino | simple-icons@16.36.0 `icons/trino.svg` | CC0-1.0 |
| `presto.svg` | Presto | simple-icons@16.36.0 `icons/presto.svg` | CC0-1.0 |
| `googlecloudspanner.svg` | Google Cloud Spanner | simple-icons@16.36.0 `icons/googlecloudspanner.svg` | CC0-1.0 |
| `apachearrow.svg` | Arrow Flight SQL | simple-icons@16.36.0 `icons/apachearrow.svg` | Apache-2.0 (as Simple Icons lists it) |
| `influxdb.svg` | InfluxDB 3 | simple-icons@16.36.0 `icons/influxdb.svg` | CC0-1.0 |
| `apachehive.svg` | Apache Hive | simple-icons@16.36.0 `icons/apachehive.svg` | Apache-2.0 (as Simple Icons lists it) |
| `apachespark.svg` | Spark SQL | simple-icons@16.36.0 `icons/apachespark.svg` | Apache-2.0 (as Simple Icons lists it) |
| `scylladb.svg` | ScyllaDB | simple-icons@16.36.0 `icons/scylladb.svg` | CC0-1.0 |
| `neo4j.svg` | Neo4j | simple-icons@16.36.0 `icons/neo4j.svg` | CC0-1.0 |
| `elasticsearch.svg` | Elasticsearch | simple-icons@16.36.0 `icons/elasticsearch.svg` | CC0-1.0 |
| `opensearch.svg` | OpenSearch | simple-icons@16.36.0 `icons/opensearch.svg` | CC0-1.0 |
| `apachesolr.svg` | Solr | simple-icons@16.36.0 `icons/apachesolr.svg` | Apache-2.0 (as Simple Icons lists it) |
| `meilisearch.svg` | Meilisearch | simple-icons@16.36.0 `icons/meilisearch.svg` | CC0-1.0 |

The marks of the files above that are one colour are drawn in the interface's text colour in
the dark theme (`.engine-icon` in `styles.css`); the Devicon originals of SQL Server and
Oracle keep their colours.

## Engines without a mark

Simple Icons 16.36.0 and Devicon 2.18.0 have none for Databend, Dremio, GizmoSQL, Apache
Kyuubi, Apache Impala and TDengine: they show the interface's own database icon (Lucide's
shape, `components/Icons.tsx`), never a made-up logo. OceanBase and Valkey have none either and
show the mark of the engine whose protocol they speak (MySQL, Redis). The CSV / Parquet folder
is not a server and shows a folder. When one of these marks reaches either project, add the
file here and move the engine to `marks` in `marks.json`.

## Devicon licence

MIT License

Copyright (c) 2015 konpa

Permission is hereby granted, free of charge, to any person obtaining a copy of this
software and associated documentation files (the "Software"), to deal in the Software
without restriction, including without limitation the rights to use, copy, modify, merge,
publish, distribute, sublicense, and/or sell copies of the Software, and to permit persons
to whom the Software is furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all copies or
substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED,
INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR
PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE
FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR
OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
DEALINGS IN THE SOFTWARE.
