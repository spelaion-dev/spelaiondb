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
  Trino on `127.0.0.1:18080` and Presto on `127.0.0.1:18081`, for tests without SSH
  (`SPELAION_TEST_CLICKHOUSE`, `SPELAION_TEST_TRINO`, `SPELAION_TEST_PRESTO`; behind the
  bastions they are `clickhouse:8123`, `trino:8080` and `presto:8080`).
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

`mysql-family/compose.yaml` (`spelaion-test-mysql-family`) has TiDB, OceanBase, SingleStore and
StarRocks for `crates/spelaion-core/tests/mysql_family.rs` (SDB-170), apart from the main
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
