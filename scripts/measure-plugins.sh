#!/usr/bin/env bash
# ADR-041's gate, measured: a million-row import with the drivers compiled in and with every
# engine through its plugin. A plugin may take at most 15 % longer. It measures the engines
# whose driver is still in this workspace: one that has left (DuckDB) was measured in the pull
# request that moved it.
#
#   scripts/measure-plugins.sh            # three runs each way, then the medians
#   RUNS=5 scripts/measure-plugins.sh
#   SPELAION_PLUGIN_ENGINES="postgres sqlite" scripts/measure-plugins.sh
#
# Optimised builds, the same one both ways, with debug assertions on: only a build with them
# follows SPELAION_DEV_PLUGIN_<ENGINE> (a release build ignores it, ADR-042). The servers are
# the usual ones (SPELAION_TEST_PG, SPELAION_TEST_MYSQL, SPELAION_TEST_MARIADB,
# SPELAION_TEST_MSSQL); an engine whose variable is not set is not measured. On a machine with
# 16 GB, set CARGO_BUILD_JOBS=3 (see CONTRIBUTING.md).
#
# What it times is what `tests/import.rs` prints (each engine loads a CSV of a million rows
# into a new table and into an existing one). Each engine is timed in a process of its own, in
# process and through its plugin by turns: what one engine leaves behind (a database being
# closed, a server still writing what it was sent) is not on the next one's clock, and neither
# way has the machine at its best. The fastest engines load a million rows in about a second,
# so 15 % is 150 ms: expect a few points of difference between two runs of this script.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"
export CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true

runs=${RUNS:-3}
engines=${SPELAION_PLUGIN_ENGINES:-"postgres mysql mariadb sqlserver sqlite"}
out=$(mktemp)
trap 'rm -f "$out"' EXIT

# The program that serves an engine: MySQL's serves MariaDB too.
program_of() {
  case "$1" in
    mariadb) echo mysql ;;
    *) echo "$1" ;;
  esac
}

packages=()
for engine in $engines; do
  package="spelaion-driver-$(program_of "$engine")"
  case " ${packages[*]:-} " in
    *" -p $package "*) ;;
    *) packages+=(-p "$package") ;;
  esac
done
echo "building (optimised)…" >&2
cargo build --release --bins "${packages[@]}" >&2
cargo test --release -p spelaion-core --test import --no-run >&2

target=$(cargo metadata --format-version 1 --no-deps | sed -E 's/.*"target_directory":"([^"]*)".*/\1/' | sed 's#\\\\#/#g')
suffix=""
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) suffix=".exe" ;;
esac
# The environment that sends each engine through its plugin.
plugins=()
for engine in $engines; do
  variable="SPELAION_DEV_PLUGIN_$(echo "$engine" | tr '[:lower:]' '[:upper:]')"
  plugins+=("$variable=$target/release/spelaion-plugin-$(program_of "$engine")$suffix")
done

# One import test, alone in its process. $1: the label, $2: the test, then the environment.
import() {
  local label=$1 test=$2
  shift 2
  env "$@" cargo test --release -p spelaion-core --test import -- --nocapture "$test" 2>&1 |
    sed -n "s/^.*\(DuckDB\|SQLite\|PostgreSQL\|MySQL\|MariaDB\|SQL Server\): 1000000 rows into \(a new\|an existing\) table in \(.*\)$/$label|\1: import into \2 table|\3/p" >> "$out"
}

# The import tests of the engines being measured.
tests=""
for engine in $engines; do
  case "$engine" in
    mysql | mariadb) test=mysql_and_mariadb_import ;;
    *) test="${engine}_imports" ;;
  esac
  case " $tests " in
    *" $test "*) ;;
    *) tests="$tests $test" ;;
  esac
done

for run in $(seq "$runs"); do
  echo "run $run of $runs…" >&2
  for test in $tests; do
    # `env` needs something to set: a variable nothing reads.
    import "in process" "$test" SPELAION_MEASURING=1
    import "plugin" "$test" "${plugins[@]}"
  done
done

# Durations as Rust prints them (1.28s, 255.5ms) in milliseconds; then the median of each
# measurement each way, and how much longer the plugin took.
awk -F'|' '
  function ms(text) {
    if (text ~ /ms$/) { sub(/ms$/, "", text); return text + 0 }
    if (text ~ /µs$/) { sub(/µs$/, "", text); return text / 1000 }
    sub(/s$/, "", text); return text * 1000
  }
  function median(key,    n, i, j, t, v) {
    n = count[key]
    for (i = 1; i <= n; i++) v[i] = value[key, i]
    for (i = 1; i <= n; i++) for (j = i + 1; j <= n; j++) if (v[j] < v[i]) { t = v[i]; v[i] = v[j]; v[j] = t }
    return n % 2 ? v[(n + 1) / 2] : (v[n / 2] + v[n / 2 + 1]) / 2
  }
  {
    key = $1 SUBSEP $2
    value[key, ++count[key]] = ms($3)
    if (!($2 in seen)) { seen[$2] = 1; order[++names] = $2 }
  }
  END {
    printf "%-42s %12s %12s %8s\n", "", "in process", "plugin", ""
    worst = 0
    for (i = 1; i <= names; i++) {
      name = order[i]
      a = median("in process" SUBSEP name); b = median("plugin" SUBSEP name)
      if (a == 0 || b == 0) continue
      more = (b / a - 1) * 100
      if (more > worst) worst = more
      printf "%-42s %9.0f ms %9.0f ms %+7.1f %%\n", name, a, b, more
    }
    printf "\nthe most a plugin adds: %+.1f %% (the gate is 15 %%)\n", worst
    exit worst > 15 ? 1 : 0
  }
' "$out"
