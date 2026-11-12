#!/usr/bin/env bash
# Clears what test runs left behind on this folder's Docker test servers: the objects tests
# create under a per-run name (`<prefix>_<process id>`, `<prefix>_<process id>_<n>`) and drop
# when they pass, which older runs that failed (before SDB-247 and SDB-254) left in place.
#
#   bash testing/docker/clean-leftovers.sh          lists what would be dropped
#   bash testing/docker/clean-leftovers.sh --drop   lists it, then drops it
#
# It only ever reaches the containers of `compose.yaml` next to it, through
# `docker compose exec`: never a host and port, never a connection someone configured. Only
# names made of a known prefix below and a numeric suffix are dropped; anything else it finds
# that is not a fixture of the compose file is listed and left alone. Run it while no tests
# use these servers: a test running meanwhile would lose its objects.
set -euo pipefail

# Git Bash would rewrite the `/opt/...` path of SQL Server's client.
export MSYS_NO_PATHCONV=1

# `pwd -W` gives Git Bash's path in the form Docker for Windows reads; elsewhere plain `pwd`.
DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && { pwd -W 2>/dev/null || pwd; })"
COMPOSE=(docker compose -f "$DIR/compose.yaml")

DROP=false
case "${1:-}" in
  "") ;;
  --drop) DROP=true ;;
  *) echo "usage: $0 [--drop]" >&2; exit 2 ;;
esac

# The prefixes of the names tests give the objects they create on the shared servers, each
# followed by `_<process id>` (and maybe `_<n>`). A test that adds a per-run name adds its
# prefix here.
PREFIXES=(
  agent_comments agent_ddl agent_ddl_schema agent_notes agent_pg_escapes agent_read
  agent_sql_mode agent_write agent_writes
  backup backup_role bk
  export_ctx export_stream export_tx export_wipe_export_stream
  gen_codes gen_customer gen_from gen_items gen_orders gen_other gen_pairs gen_upsert
  import_cancel import_existing import_fail import_new
  no_such_schema query_pages read_only read_only_pg
  runs runs_all runs_pause runs_smart runs_xact
  setup_app setup_ro
  spelaion_call spelaion_ddl spelaion_like spelaion_monitor spelaion_objects spelaion_seq
  spelaion_test spelaion_zoo
  tx_end tx_lost tx_manual tx_metadata tx_myisam tx_smart
  type_zoo
)
LEFTOVER="^($(IFS='|'; echo "${PREFIXES[*]}"))_[0-9]+(_[0-9]+)?(_copy)?$"

# What the compose file and the servers themselves create, or the tests keep on purpose
# (created when missing, under a fixed name): listed as neither a leftover nor unknown.
KEPT="^(spelaion|spelaion_gis|spelaion_cert|spelaion_contained|spelaion_contained_user|spelaion_edit_source|postgres|public|big|sa|root|healthcheck|bastion|mysql|sys|information_schema|performance_schema|master|msdb|model|tempdb)$"

# Objects tests create under a fixed name and replace on every run (one copy at most, so
# nothing piles up), and the servers' own accounts: kept, and not listed as unknown.
FIXED="^(mask_[a-z]+|erd_[a-z]+|cs_[a-z]+|core_eq[a-z_]*|core_edits|smoke_[a-z_]+|sdb262_comment|zoo|like_probe|call_probe|explorer|ddl|two_sets|mysql[.](infoschema|session|sys)|mariadb[.]sys|##MS_.*##|NT AUTHORITY.*)$"

running() {
  "${COMPOSE[@]}" ps --status running --services 2>/dev/null | grep -qx "$1"
}

pg() { "${COMPOSE[@]}" exec -T postgres psql -U spelaion -v ON_ERROR_STOP=0 -Atq "$@"; }
my() { "${COMPOSE[@]}" exec -T -e MYSQL_PWD=spelaion-root "$1" "$2" -uroot -N -B --force; }
ms() {
  "${COMPOSE[@]}" exec -T sqlserver /opt/mssql-tools18/bin/sqlcmd -C -S localhost -U sa \
    -P Spelaion-test-1 -h -1 -W -s "|" "$@"
}

# Reads `kind|schema|name[|extra]` lines; prints the leftovers' (named with a known prefix)
# unchanged on stdout, and the names that are neither leftovers nor kept on stderr. An object in
# a leftover schema or database goes with it, so it is not printed.
classify() {
  local kind schema name extra
  while IFS='|' read -r kind schema name extra; do
    [ -n "$name" ] || continue
    if [[ "$name" =~ $LEFTOVER ]]; then
      echo "$kind|$schema|$name|$extra"
    elif [[ -n "$schema" && ( "$schema" =~ $LEFTOVER || "$schema" =~ $FIXED ) ]]; then
      : # goes with its schema or database, or is kept with it
    elif [[ ! "$name" =~ $KEPT && ! "$name" =~ $FIXED ]]; then
      echo "  not a known test object, left alone: $kind ${schema:+$schema.}$name" >&2
    fi
  done
}

# Prints the statements, then runs them with `$@` if --drop was given.
apply() {
  local label="$1" statements="$2"
  shift 2
  local count
  count=$(printf '%s' "$statements" | grep -c . || true)
  echo "$label: $count to drop"
  [ "$count" -gt 0 ] || return 0
  printf '%s\n' "$statements" | sed 's/^/  /'
  if $DROP; then
    printf '%s\n' "$statements" | "$@" >/dev/null || echo "  (some drops failed: see above)"
  fi
}

postgres() {
  local found statements dbs
  found=$(pg -d spelaion -F '|' -c "
    select 'database', '', datname, '' from pg_database where not datistemplate
    union all select 'schema', '', nspname, '' from pg_namespace
      where nspname !~ '^pg_' and nspname <> 'information_schema'
    union all select case c.relkind when 'v' then 'VIEW' when 'm' then 'MATERIALIZED VIEW'
        when 'S' then 'SEQUENCE' when 'f' then 'FOREIGN TABLE' else 'TABLE' end,
        n.nspname, c.relname, ''
      from pg_class c join pg_namespace n on n.oid = c.relnamespace
      where c.relkind in ('r', 'p', 'v', 'm', 'S', 'f') and n.nspname !~ '^pg_'
        and n.nspname <> 'information_schema'
        and not exists (select 1 from pg_depend d
          where d.classid = 'pg_class'::regclass and d.objid = c.oid and d.deptype in ('a', 'i', 'e'))
    union all select 'ROUTINE', n.nspname, p.proname, pg_get_function_identity_arguments(p.oid)
      from pg_proc p join pg_namespace n on n.oid = p.pronamespace
      where n.nspname !~ '^pg_' and n.nspname <> 'information_schema'
        and not exists (select 1 from pg_depend d
          where d.classid = 'pg_proc'::regclass and d.objid = p.oid and d.deptype = 'e')
    union all select 'role', '', rolname, '' from pg_roles where rolname !~ '^pg_'")
  statements=$(printf '%s
' "$found" | classify |
    while IFS='|' read -r kind schema name extra; do
      case "$kind" in
        database) ;;
        schema) echo "DROP SCHEMA IF EXISTS \"$name\" CASCADE;" ;;
        ROUTINE) echo "DROP ROUTINE IF EXISTS \"$schema\".\"$name\"($extra) CASCADE;" ;;
        role) echo "DROP OWNED BY \"$name\" CASCADE; DROP ROLE IF EXISTS \"$name\";" ;;
        *) echo "DROP $kind IF EXISTS \"$schema\".\"$name\" CASCADE;" ;;
      esac
    done |
    # Roles last: what they own goes with the schemas and tables first.
    awk '/^DROP OWNED/ { r[++n] = $0; next } { print } END { for (i = 1; i <= n; i++) print r[i] }')
  apply "postgres (spelaion)" "$statements" pg -d spelaion
  dbs=$(printf '%s
' "$found" | grep '^database|' | classify | cut -d'|' -f3 |
    sed 's/.*/DROP DATABASE IF EXISTS "&" WITH (FORCE);/' || true)
  apply "postgres (databases)" "$dbs" pg -d postgres
}

mysqlish() {
  local service="$1" client="$2" found statements
  found=$(my "$service" "$client" <<'SQL'
select 'database', '', schema_name from information_schema.schemata;
select lower(table_type), table_schema, table_name from information_schema.tables
  where table_schema not in ('mysql', 'information_schema', 'performance_schema', 'sys');
select lower(routine_type), routine_schema, routine_name from information_schema.routines
  where routine_schema not in ('mysql', 'sys');
select 'user', host, user from mysql.user;
SQL
  )
  statements=$(printf '%s\n' "$found" | tr '\t' '|' | classify |
    while IFS='|' read -r kind schema name _; do
      case "$kind" in
        database) echo "DROP DATABASE IF EXISTS \`$name\`;" ;;
        "base table" | "system versioned") echo "DROP TABLE IF EXISTS \`$schema\`.\`$name\`;" ;;
        view) echo "DROP VIEW IF EXISTS \`$schema\`.\`$name\`;" ;;
        function) echo "DROP FUNCTION IF EXISTS \`$schema\`.\`$name\`;" ;;
        procedure) echo "DROP PROCEDURE IF EXISTS \`$schema\`.\`$name\`;" ;;
        user) echo "DROP USER IF EXISTS '$name'@'$schema';" ;;
        *) echo "-- $kind $schema.$name: no drop for this kind" ;;
      esac
    done)
  apply "$service" "$statements" my "$service" "$client"
}

sqlserver() {
  local found statements
  found=$(ms -Q "set nocount on;
    select 'database', '', name from sys.databases;
    select 'login', '', name from sys.server_principals where type in ('S', 'U');
    if db_id('spelaion') is not null exec ('use spelaion;
      select ''schema'', '''', name from sys.schemas where schema_id between 5 and 16383;
      select case o.type when ''U'' then ''table'' when ''V'' then ''view''
          when ''P'' then ''procedure'' when ''SN'' then ''synonym'' else ''function'' end,
        s.name, o.name
        from sys.objects o join sys.schemas s on s.schema_id = o.schema_id
        where o.is_ms_shipped = 0 and o.type in (''U'', ''V'', ''P'', ''FN'', ''IF'', ''TF'', ''SN'');
      select ''user'', '''', name from sys.database_principals
        where type in (''S'', ''U'') and principal_id > 4;')")
  statements=$(printf '%s\n' "$found" | tr -d '\r' | classify |
    while IFS='|' read -r kind schema name _; do
      case "$kind" in
        database) echo "ALTER DATABASE [$name] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE IF EXISTS [$name];" ;;
        login) echo "DROP LOGIN [$name];" ;;
        schema) echo "USE spelaion; DROP SCHEMA IF EXISTS [$name];" ;;
        table) echo "USE spelaion; DROP TABLE IF EXISTS [$schema].[$name];" ;;
        view) echo "USE spelaion; DROP VIEW IF EXISTS [$schema].[$name];" ;;
        procedure) echo "USE spelaion; DROP PROCEDURE IF EXISTS [$schema].[$name];" ;;
        function) echo "USE spelaion; DROP FUNCTION IF EXISTS [$schema].[$name];" ;;
        synonym) echo "USE spelaion; DROP SYNONYM IF EXISTS [$schema].[$name];" ;;
        user) echo "USE spelaion; DROP USER IF EXISTS [$name];" ;;
      esac
    done |
    # Objects before the schemas and users that hold them, logins and databases last.
    awk '/DROP (TABLE|VIEW|PROCEDURE|FUNCTION|SYNONYM)/ { a[++na] = $0; next }
         /DROP (SCHEMA|USER)/ { b[++nb] = $0; next }
         { c[++nc] = $0 }
         END { for (i = 1; i <= na; i++) print a[i]; for (i = 1; i <= nb; i++) print b[i];
               for (i = 1; i <= nc; i++) print c[i] }')
  apply "sqlserver" "$statements" ms -i /dev/stdin
}

for service in postgres mysql mariadb sqlserver; do
  if ! running "$service"; then
    echo "$service is not running in $DIR/compose.yaml: skipped" >&2
    continue
  fi
  case "$service" in
    postgres) postgres ;;
    mysql) mysqlish mysql mysql ;;
    mariadb) mysqlish mariadb mariadb ;;
    sqlserver) sqlserver ;;
  esac
done
if ! $DROP; then
  echo "Nothing was dropped. Run again with --drop, while no tests use these servers, to drop it."
fi
