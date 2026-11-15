#!/bin/bash
# The pinned lite image invokes START_FILE after unpacking/linking its pre-built database,
# before the first startup. INIT_SGA_SIZE/INIT_PGA_SIZE do not configure that database.
set -euo pipefail

vendor_start="$ORACLE_BASE/oracle-free-26ai"
if [ "${1:-}" != start ]; then
  exec "$vendor_start" "$@"
fi

work=$(mktemp -d)
trap 'rm -f "$work/original.ora" "$work/ci.ora"; rmdir "$work"' EXIT
spfile="$ORACLE_HOME/dbs/spfile${ORACLE_SID}.ora"

# SQL*Plus can convert the existing spfile while the instance is idle: retain the vendor's
# database paths and settings instead of maintaining a second, incomplete init file.
sqlplus -L -s / as sysdba <<SQL
whenever oserror exit failure
whenever sqlerror exit failure
create pfile='$work/original.ora' from spfile='$spfile';
exit
SQL

# Discard saved automatic component sizes too, so they do not force the old, larger SGA.
awk '
  /^[[:space:]]*[^.]+\.__/ { next }
  tolower($0) ~ /^[[:space:]]*[^.]+\.(memory_target|memory_max_target|sga_target|sga_max_size|pga_aggregate_target|pga_aggregate_limit)[[:space:]]*=/ { next }
  { print }
' "$work/original.ora" > "$work/ci.ora"
cat >> "$work/ci.ora" <<'PARAMETERS'
*.memory_target=0
*.memory_max_target=0
*.sga_target=768M
*.sga_max_size=768M
*.pga_aggregate_target=256M
*.pga_aggregate_limit=2G
PARAMETERS

# Oracle's minimum PGA hard limit is 2 GiB; the 256 MiB target sizes its normal work.
# The container's 2 GiB limit bounds the whole database, including native allocations.
sqlplus -L -s / as sysdba <<SQL
whenever oserror exit failure
whenever sqlerror exit failure
create spfile='$spfile' from pfile='$work/ci.ora';
exit
SQL
echo 'Oracle CI memory: SGA 768 MiB, PGA target 256 MiB, container limit 2 GiB'
rm -f "$work/original.ora" "$work/ci.ora"
rmdir "$work"
trap - EXIT
exec "$vendor_start" "$@"
