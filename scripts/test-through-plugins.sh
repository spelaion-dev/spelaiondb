#!/usr/bin/env bash
# Runs the tests with every engine served by its plugin instead of the driver compiled in
# (ADR-041's gate: an engine's whole test set passes through its plugin, unchanged).
#
# It builds the plugin programs of this workspace (`spelaion-plugin-<engine>`) and points a
# development build at them with `SPELAION_DEV_PLUGIN_<ENGINE>`; a release build ignores those
# variables. Everything after the script's name goes to `cargo test`:
#
#   scripts/test-through-plugins.sh --workspace
#   scripts/test-through-plugins.sh -p spelaion-core --test transactions
#
# The engines are the ones whose driver is still in this workspace. DuckDB's is not (it is a
# plugin of its own repository): its tests always go through its plugin.
# `SPELAION_PLUGIN_ENGINES` narrows it to some engines:
#
#   SPELAION_PLUGIN_ENGINES="postgres sqlite" scripts/test-through-plugins.sh -p spelaion-core
#
# The servers the tests need are the usual ones (`SPELAION_TEST_PG` and the rest, see
# `AGENTS.md`); without them those tests are skipped, here as anywhere.
set -euo pipefail

root=$(git rev-parse --show-toplevel)
cd "$root"

engines=${SPELAION_PLUGIN_ENGINES:-"postgres sqlite"}

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
cargo build --bins "${packages[@]}"

target=$(cargo metadata --format-version 1 --no-deps | sed -E 's/.*"target_directory":"([^"]*)".*/\1/' | sed 's#\\\\#/#g')
suffix=""
case "$(uname -s)" in
  MINGW* | MSYS* | CYGWIN*) suffix=".exe" ;;
esac

for engine in $engines; do
  program="$target/debug/spelaion-plugin-$(program_of "$engine")$suffix"
  test -f "$program" || { echo "missing $program" >&2; exit 1; }
  variable="SPELAION_DEV_PLUGIN_$(echo "$engine" | tr '[:lower:]' '[:upper:]')"
  export "$variable=$program"
  echo "$variable=$program"
done

exec cargo test "$@"
