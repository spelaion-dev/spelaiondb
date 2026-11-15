# Test environment

PostgreSQL 17, MySQL 8.4, MariaDB 11.4 and SQL Server 2022 behind two chained SSH bastions:

```
host ──127.0.0.1:2221──► bastion1 ──(mid network)──► bastion2 ──(data network)──► postgres:5432
                                                                                  mysql:3306
                                                                                  mariadb:3306
                                                                                  sqlserver:1433
```

- The bastions only allow port forwarding (`direct-tcpip`): they give no shell.
- `proxy` (ADR-027): SOCKS5 (Dante) on `127.0.0.1:11080` and HTTP `CONNECT` (tinyproxy) on
  `127.0.0.1:13128`, user `proxyuser`, password `proxy-pass`. It is on the bastions' and the
  databases' networks, so it reaches `bastion2` and `postgres` by name. Tests:
  `SPELAION_TEST_SOCKS=127.0.0.1:11080 SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128`.
- `bastion2` and `postgres` only resolve from inside the chain.
- Postgres is also published on `127.0.0.1:15432`, MySQL on `127.0.0.1:13306`, MariaDB
  on `127.0.0.1:13307`, SQL Server on `127.0.0.1:11433`, ClickHouse (HTTP) on `127.0.0.1:18123`,
  Trino on `127.0.0.1:18080`, Presto on `127.0.0.1:18081` and GizmoSQL (Flight SQL) on
  `127.0.0.1:31337`, for tests without SSH
  (`SPELAION_TEST_CLICKHOUSE`, `SPELAION_TEST_TRINO`, `SPELAION_TEST_PRESTO`,
  `SPELAION_TEST_GIZMOSQL`; behind the
  bastions they are `clickhouse:8123`, `trino:8080`, `presto:8080` and `gizmosql:31337`).
- Oracle Database Free (Oracle's lite image, about 2 GB; a minute or two to start the first
  time) is on `127.0.0.1:11521` (`SPELAION_TEST_ORACLE`; behind the bastions `oracle:1521`):
  user and password `spelaion`, service `FREEPDB1`, made by `oracle/startup` on every start.
  Its TCPS is a TLS proxy in front of it (the lite image has no `orapki`), on
  `127.0.0.1:12484` and `oracle-tls:2484`, with a certificate for `localhost` and
  `oracle-tls` signed by `keys/ca.crt` (`gen-keys.sh`).
- Apache Hive 4.0.1's HiveServer2 (with its embedded metastore; about 3 GB, half a minute to
  start) is on `127.0.0.1:11010` without authentication (`SPELAION_TEST_HIVE`; behind the
  bastions `hive:10000`). Its JVM is bounded (`mem_limit`), like every new JVM service here.
  Spark's Thrift server, Kyuubi and Impala are tested in the HiveServer2 plugin's repository.
- libSQL's server (`sqld` 0.24.33) is on `127.0.0.1:18095` without authentication
  (`SPELAION_TEST_LIBSQL`; behind the bastions and the proxies `libsql:8080`). Tokens and TLS
  are tested in its plugin's repository.
- Neo4j 5.26 (community) is on `127.0.0.1:17474`, its HTTP Query API, user `neo4j`, password
  `spelaion-test` (`SPELAION_TEST_NEO4J`; behind the bastions and the proxies `neo4j:7474`).
  TLS is tested in its plugin's repository.
- Databend 1.2.881 is on `127.0.0.1:18320`, its HTTP query API, user `spelaion`, password
  `spelaion-test` (`SPELAION_TEST_DATABEND`; behind the bastions and the proxies
  `databend:8000`). TLS is tested in its plugin's repository.
- Google's Spanner emulator 1.5.28 is on `127.0.0.1:19021`, its REST API, without a login
  (`SPELAION_TEST_SPANNER`; behind the bastions and the proxies `spanner:9020`). The tests
  create its instance and database; TLS is tested in its plugin's repository.
- SQL Server (Developer edition, Microsoft's image `mcr.microsoft.com/mssql/server:2022-latest`,
  about 1.5 GB): user `sa`, password `Spelaion-test-1`; the tests create the `spelaion`
  database. It encrypts with its own self-signed certificate.
- SSH user: `spelaion`. Every database: user `spelaion`, password `spelaion`, database
  `spelaion`.
- `sqlitehost` (reached through bastion1) holds `/home/spelaion/shop.db` and `other.db`, and
  lets the client run commands (its `sqlite3`), for SQLite on a server (ADR-018).
- MySQL and MariaDB accept TLS with `keys/mysql.crt` (valid for `localhost`, `mysql` and
  `mariadb`).
- The `big` table has 1 M rows with varied types.
- PostgreSQL is the PostGIS image (`postgis/postgis:17-3.5-alpine`), but only the
  `spelaion_gis` database has the extension (`postgres-init/02-postgis.sql`), for the map
  view's tests (ADR-045); `spelaion` stays plain. An older container made from
  `postgres:17-alpine` has no `spelaion_gis`: recreate it with
  `docker compose -f testing/docker/compose.yaml up -d --force-recreate --renew-anon-volumes --wait postgres`
  (the image keeps its data in an anonymous volume, so without `--renew-anon-volumes` the init
  scripts do not run again).
- PostgreSQL accepts TLS (without requiring it) with a certificate signed by `keys/ca.crt`,
  valid for `localhost` and `postgres` (not for `127.0.0.1`): that way `verify-full` and
  `verify-ca` are tested, also through the bastions.
- The user `spelaion_cert` logs in with the client certificate `keys/client.crt` and no
  password (ADR-022): PostgreSQL through `postgres/pg_hba.conf`, MySQL and MariaDB through
  `REQUIRE SUBJECT` (the driver tests create the user).

## Usage

```bash
bash testing/docker/gen-keys.sh
docker compose -f testing/docker/compose.yaml up -d --build --wait
```

Generated keys (test values, outside git):
- `keys/id_ed25519`: no passphrase.
- `keys/id_ed25519_pass`: passphrase `spelaion-test`.
- `keys/ca.crt`, `keys/server.crt`, `keys/server.key`: PostgreSQL TLS.
- `keys/client.crt`, `keys/client.key`: client certificate for `spelaion_cert`.

Manual check with OpenSSH:

```bash
ssh -i testing/docker/keys/id_ed25519 -J spelaion@127.0.0.1:2221 -p 22 spelaion@bastion2 -N -L 15432:postgres:5432
```

## CI memory budget

The main Compose project has a hard limit on every container. CI uses two phases: the default
twenty services total **11,776 MiB (11.5 GiB)**; after they stop, Hive and the two bastions
total **1,664 MiB (1.625 GiB)**. Running all twenty-one at once would total 13 GiB and leave
too little room for compilation. Public GitHub `ubuntu-latest` runners have
[16 GB of RAM](https://docs.github.com/en/actions/reference/runners/github-hosted-runners#standard-github-hosted-runners-for-public-repositories).
Before starting either CI job's services, `check-memory.py` checks the resolved Compose JSON:
no unbounded service (even inactive profiles), at most 11.5 GiB per active phase, and **at
least 4 GB (3,815 MiB) of the Docker
host's actual RAM left** for the operating system, the kind cluster and Rust compilation/tests.
This uses the daemon's reported RAM: a real runner reported 15,700 MiB after kernel reservations,
which leaves 3,924 MiB (4.11 GB) after the services, rather than assuming it has 16 GiB available.
The smaller protocol-family projects below are separate, opt-in projects; CI does not add
them to this budget.

| Container | Limit (MiB) | Engine allocation below that limit |
|---|---:|---|
| Trino, Presto (each) | 1536 | 1024 MiB heap, 128 MiB direct buffers, 64 MiB code cache |
| Cassandra | 1024 | 256 MiB heap, 128 MiB direct buffers |
| Neo4j | 768 | 384 MiB heap, 64 MiB direct buffers, 128 MiB page cache |
| Oracle | 2048 | Native database: 768 MiB SGA, 256 MiB PGA target; container cap includes native work |
| SQL Server | 2048 | 1280 MiB memory manager limit |
| MongoDB | 512 | 256 MiB WiredTiger cache |
| ClickHouse | 512 | 384 MiB server memory limit; 64 MiB mark / 16 MiB uncompressed caches |
| PostgreSQL | 256 | 64 MiB shared buffers, 1 MiB work memory, 16 MiB maintenance memory; no parallel query workers |
| MySQL | 384 | 64 MiB InnoDB buffer pool; smaller performance schema tables |
| MariaDB | 256 | 64 MiB InnoDB buffer pool |
| GizmoSQL | 256 | Container limit |
| libSQL, proxy, bastion1, bastion2, sqlitehost (each) | 64 | Container limit |
| Redis | 32 | Container limit |
| oracle-tls | 32 | Container limit |
| TDengine | 256 | Container limit |
| Hive (separate phase) | 1536 | 768 MiB heap, 128 MiB direct buffers, 64 MiB code cache |

Hive has the opt-in Compose profile `hive`. The required Linux job runs the workspace with
`SPELAION_TEST_HIVE` unset, then stops its default services, starts `hive bastion1 bastion2`
and runs the complete `spelaion-core --test hive` suite with its real server and SSH variables.
This is part of the same required check; a failure fails that check. Linux smoke does not use
Hive and runs the default phase only. The budget checker reads `config --profile '*'` so
every service must have a limit, and takes the exact service names for the second phase.

For local testing on a machine with the same memory, finish the default workspace tests first.
On a Compose project used only by your test run, stop that phase and run Hive:

```bash
docker compose -f testing/docker/compose.yaml stop
docker compose -f testing/docker/compose.yaml up -d --build --wait hive bastion1 bastion2
SPELAION_TEST_HIVE=127.0.0.1:11010 SPELAION_TEST_SSH=127.0.0.1:2221 \
  cargo test -p spelaion-core --test hive
```

The JVMs see two processors so thread pools do not grow with a developer machine's CPU count.
Trino and Presto use their pinned images' launch flags in the mounted `jvm.config` files,
with heap/direct limits and a smaller code cache; changing an image means checking its flags
again. Their task concurrency is two, with four worker threads, and query history targets ten
plans once they are 30 seconds old. Readiness alone is insufficient when tuning them: run
the parallel `trino.rs` integration tests, including the sorted export and cancellation workload. Keep space for
native allocations, health checks and query execution when changing a limit.
HAProxy also has two threads and 128 connections, so neither CPUs nor a high file-descriptor
limit sizes its TLS pools.

Oracle's lite image carries a pre-built database; its startup does not apply `INIT_SGA_SIZE`
or `INIT_PGA_SIZE`. `oracle/start-ci.sh` uses the image's `START_FILE` hook, after extraction
and before its first startup, to convert the vendor spfile through SQL*Plus, replace only
memory settings (including saved automatic component sizes), and delegate to the vendor's
normal startup. The image is pinned by digest because that hook depends on its layout.
The SGA is 768 MiB and the PGA target is 256 MiB; Oracle's
[minimum PGA hard limit](https://docs.oracle.com/en/database/oracle/oracle-database/26/refrn/PGA_AGGREGATE_LIMIT.html) is 2 GiB,
and the container cap bounds the complete process group. A cap alone can make its default
working set thrash without being killed, so validate clean boots and effective parameters.

CI bounds Compose's whole startup, including image pulls/builds, at 15 minutes (then 30
seconds to force termination). The readiness step is bounded at 12 minutes, with a 10-minute
wait for its status file; diagnostics after failure are bounded at two minutes.

On startup failure both CI jobs capture `docker stats --no-stream --all`, container limits
and state (including `OOMKilled`), and the tail of each service's log immediately in
`services.log`. The wait step prints it and captures fresh diagnostics if it fails or times
out. Both jobs also capture diagnostics if a later test fails, so a server that dies after
readiness is visible. To inspect a local failure, run `bash testing/docker/diagnostics.sh`.

## Servers that speak PostgreSQL's protocol

`pg-family/compose.yaml` is a project of its own (`spelaion-test-pg-family`) with CockroachDB,
YugabyteDB, TimescaleDB, RisingWave, Materialize and CrateDB, each as its image starts and
without a password, for `crates/spelaion-core/tests/postgres_family.rs` (SDB-169). Its images
are large (RisingWave's is about 11 GB), so nothing starts them unless asked, and CI does not.
The ports and variables are at the top of the file.

```bash
docker compose -f testing/docker/pg-family/compose.yaml up -d --wait
```

## Servers that speak MySQL's protocol

`mysql-family/compose.yaml` (`spelaion-test-mysql-family`) has TiDB, OceanBase, SingleStore,
StarRocks and Databend for `crates/spelaion-core/tests/mysql_family.rs` (SDB-170), apart from the main
environment and CI for the same reason (StarRocks' image is about 8 GB). The ports and
variables are at the top of the file.

```bash
docker compose -f testing/docker/mysql-family/compose.yaml up -d --wait
```

## GreptimeDB's ineffective read-only transactions

`greptime/compose.yaml` runs GreptimeDB 1.3.1 for the barrier regression tests in
`mysql_family.rs` and `postgres_family.rs` (SDB-343). Both protocols accept a read-only
transaction and still commit an `INSERT`. The tests verify that the core refuses read-only
connections, agents' reads and streaming reads on this server. They also send a write directly
to each plugin to check why that refusal is necessary. This fixture does not enable general
GreptimeDB support: its explorer still needs plugin changes.

```bash
docker compose -f testing/docker/greptime/compose.yaml up -d --wait
SPELAION_TEST_GREPTIME_MYSQL=127.0.0.1:18402 SPELAION_TEST_GREPTIME_PG=127.0.0.1:18403 \
  cargo test -p spelaion-core --test mysql_family --test postgres_family
docker compose -f testing/docker/greptime/compose.yaml down
```

Use the pinned plugins, or set `SPELAION_DEV_PLUGIN_MYSQL` and `SPELAION_DEV_PLUGIN_POSTGRES`
to the programs downloaded by `spelaion-plugin-fetch`.

## Clearing test leftovers

Tests that create objects on these servers give them a per-run name (`<prefix>_<process id>`,
sometimes `_<n>` after it) and drop them even when they fail. Runs from before that guard
(SDB-247, SDB-254), or a run that was killed, can leave some behind. `clean-leftovers.sh`
lists them, and drops them with `--drop`:

```bash
bash testing/docker/clean-leftovers.sh
bash testing/docker/clean-leftovers.sh --drop
```

- It only reaches the containers of this folder's `compose.yaml`, through `docker compose exec`
  (never a host and port), and only drops names made of a prefix in its `PREFIXES` list and a
  numeric suffix: databases, schemas, tables, views, sequences, functions and procedures, roles,
  users and logins. A test that adds a per-run name adds its prefix there.
- It prints every statement before running it. What it finds that is neither a leftover, a
  fixture of the compose file nor an object a test keeps under a fixed name is listed as "not a
  known test object" and left alone.
- Run it while no tests use these servers: a test running meanwhile would lose its objects.
