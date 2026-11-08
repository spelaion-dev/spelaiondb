#!/usr/bin/env bash
# Captures the MySQL and MariaDB execution-plan fixtures from the project's Docker servers
# (testing/docker/compose.yaml). Each server gets its own uniquely named, empty database that
# holds only the synthetic data of setup.sql; it is dropped at the end, even on failure.
# Every <name>.sql in mysql/ and mariadb/ is run through the server's own client in batch,
# raw mode, and its single result cell is written unchanged to <name>.json or <name>.txt
# (the client's own trailing newline removed). provenance.json records versions and hashes.
#
#   bash crates/spelaion-core/tests/fixtures/plans/capture-mysql.sh [compose file]
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../../../.." && pwd)"
compose="${1:-$root/testing/docker/compose.yaml}"
suffix="$(od -An -N8 -tx1 /dev/urandom | tr -d ' \n')"
database="spelaion_plan47_$suffix"

capture() {
  local service="$1" client="$2" dir="$here/$1"
  local container
  container="$(docker compose -f "$compose" ps -q "$service")"
  [[ "$container" =~ ^[a-f0-9]{12,64}$ ]] || { echo "no $service container" >&2; return 1; }
  local run=(docker exec -i "$container" "$client" -uroot -pspelaion-root --batch --raw --skip-column-names)
  "${run[@]}" -e "CREATE DATABASE $database" 2>/dev/null
  trap '"${run[@]}" -e "DROP DATABASE IF EXISTS $database" 2>/dev/null || true' RETURN
  "${run[@]}" "$database" < "$dir/setup.sql" > /dev/null 2>&1
  local version image entries=()
  version="$("${run[@]}" -e "SELECT VERSION()" 2>/dev/null | tr -d '\r\n')"
  image="$(docker inspect --format '{{.Image}}' "$container")"
  for sql in "$dir"/*.sql; do
    local name
    name="$(basename "$sql" .sql)"
    [[ "$name" == setup ]] && continue
    local out="$dir/$name.json"
    [[ "$name" == analyze-* && "$service" == mysql ]] && out="$dir/$name.txt"
    "${run[@]}" "$database" < "$sql" 2>/dev/null | head -c -1 > "$out"
    [[ -s "$out" ]] || { echo "empty capture: $out" >&2; return 1; }
    entries+=("{\"name\":\"$name\",\"sql\":\"$name.sql\",\"output\":\"$(basename "$out")\",\"bytes\":$(wc -c < "$out"),\"sha256\":\"$(sha256sum "$out" | cut -d' ' -f1)\"}")
  done
  local joined
  joined="$(IFS=,; echo "${entries[*]}")"
  printf '{"captured_at_utc":"%s","generated_by":"capture-mysql.sh","compose_file":"testing/docker/compose.yaml","service":"%s","server_version":"%s","image":"%s","synthetic_customers":1000,"synthetic_orders":120000,"owned_temporary_database":"%s","database_dropped":true,"fixtures":[%s]}\n' \
    "$(date -u +%Y-%m-%dT%H:%M:%SZ)" "$service" "$version" "$image" "$database" "$joined" > "$dir/provenance.json"
}

capture mysql mysql
capture mariadb mariadb
echo "captured into $here/mysql and $here/mariadb ($database dropped)"
