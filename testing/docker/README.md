# Test environment

PostgreSQL 17, MySQL 8.4, MariaDB 11.4 and SQL Server 2022 behind two chained SSH bastions:

```
host ──127.0.0.1:2221──► bastion1 ──(mid network)──► bastion2 ──(data network)──► postgres:5432
                                                                                  mysql:3306
                                                                                  mariadb:3306
                                                                                  sqlserver:1433
```

- The bastions only allow port forwarding (`direct-tcpip`): they give no shell.
- `bastion2` and `postgres` only resolve from inside the chain.
- Postgres is also published on `127.0.0.1:15432`, MySQL on `127.0.0.1:13306`, MariaDB
  on `127.0.0.1:13307` and SQL Server on `127.0.0.1:11433`, for tests without SSH.
- SQL Server (Developer edition, Microsoft's image `mcr.microsoft.com/mssql/server:2022-latest`,
  about 1.5 GB): user `sa`, password `Spelunk-test-1`; the tests create the `spelunk`
  database. It encrypts with its own self-signed certificate.
- SSH user: `spelunk`. Every database: user `spelunk`, password `spelunk`, database
  `spelunk`.
- `sqlitehost` (reached through bastion1) holds `/home/spelunk/shop.db` and `other.db`, and
  lets the client run commands (its `sqlite3`), for SQLite on a server (ADR-018).
- MySQL and MariaDB accept TLS with `keys/mysql.crt` (valid for `localhost`, `mysql` and
  `mariadb`).
- The `big` table has 1 M rows with varied types.
- PostgreSQL accepts TLS (without requiring it) with a certificate signed by `keys/ca.crt`,
  valid for `localhost` and `postgres` (not for `127.0.0.1`): that way `verify-full` and
  `verify-ca` are tested, also through the bastions.
- The user `spelunk_cert` logs in with the client certificate `keys/client.crt` and no
  password (ADR-022): PostgreSQL through `postgres/pg_hba.conf`, MySQL and MariaDB through
  `REQUIRE SUBJECT` (the driver tests create the user).

## Usage

```bash
bash testing/docker/gen-keys.sh
docker compose -f testing/docker/compose.yaml up -d --build --wait
```

Generated keys (test values, outside git):
- `keys/id_ed25519`: no passphrase.
- `keys/id_ed25519_pass`: passphrase `spelunk-test`.
- `keys/ca.crt`, `keys/server.crt`, `keys/server.key`: PostgreSQL TLS.
- `keys/client.crt`, `keys/client.key`: client certificate for `spelunk_cert`.

Manual check with OpenSSH:

```bash
ssh -i testing/docker/keys/id_ed25519 -J spelunk@127.0.0.1:2221 -p 22 spelunk@bastion2 -N -L 15432:postgres:5432
```
