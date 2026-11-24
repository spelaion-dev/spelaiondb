#!/bin/bash
# Starts Babelfish (SDB-313). The first time, it creates the cluster, a self-signed certificate
# so the TDS login can be encrypted, and the Babelfish database in multi-db mode, owned by the
# login the tests use (`babelfish_user`, the same password as the SQL Server container's).
set -euo pipefail

if [ ! -s "$PGDATA/PG_VERSION" ]; then
    # Babelfish's collations are ICU's, which need a UTF-8 database. Its TDS login checks
    # passwords with md5, not SCRAM: the test login's password is stored that way.
    initdb -D "$PGDATA" -U postgres --encoding=UTF8 --locale=C.UTF-8 \
        --auth-local=trust --auth-host=md5 >/dev/null
    openssl req -new -x509 -days 3650 -nodes -subj "/CN=babelfish" \
        -keyout "$PGDATA/server.key" -out "$PGDATA/server.crt" 2>/dev/null
    chmod 600 "$PGDATA/server.key"
    {
        echo "listen_addresses = '*'"
        echo "shared_preload_libraries = 'babelfishpg_tds'"
        echo "babelfishpg_tds.listen_addresses = '*'"
        echo "ssl = on"
        echo "ssl_cert_file = 'server.crt'"
        echo "ssl_key_file = 'server.key'"
        echo "password_encryption = md5"
    } >> "$PGDATA/postgresql.conf"
    echo "host all all 0.0.0.0/0 md5" >> "$PGDATA/pg_hba.conf"

    pg_ctl -D "$PGDATA" -w -l "$PGDATA/setup.log" start >/dev/null
    psql -v ON_ERROR_STOP=1 -U postgres -d postgres -q <<'SQL'
CREATE USER babelfish_user WITH CREATEDB CREATEROLE PASSWORD 'Spelaion-test-1' INHERIT;
CREATE DATABASE babelfish_db OWNER babelfish_user;
SQL
    psql -v ON_ERROR_STOP=1 -U postgres -d babelfish_db -q <<'SQL'
CREATE EXTENSION IF NOT EXISTS "babelfishpg_tds" CASCADE;
GRANT ALL ON SCHEMA sys TO babelfish_user;
ALTER SYSTEM SET babelfishpg_tsql.database_name = 'babelfish_db';
ALTER DATABASE babelfish_db SET babelfishpg_tsql.migration_mode = 'multi-db';
SELECT pg_reload_conf();
CALL sys.initialize_babelfish('babelfish_user');
SQL
    pg_ctl -D "$PGDATA" -w stop >/dev/null
fi

exec postgres -D "$PGDATA"
