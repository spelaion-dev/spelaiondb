#!/usr/bin/env bash
# Tests system-packages.sh (SDB-427) with a fake apt-get: a slow install that ends inside the
# wait passes, a failed one fails saying apt failed, one still running when the wait ends fails
# saying it timed out, and apt is asked to retry. Also checks that both workflows that install
# packages wait the same, longer than the ten minutes a slow mirror once took. ci.yml's Linux job
# runs it first; locally: `bash .github/workflows/system-packages.test.sh`.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script="$here/system-packages.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

# The fake apt-get: logs its arguments, sleeps FAKE_APT_SLEEP seconds on `install`, and exits
# with FAKE_APT_EXIT there.
cat > "$work/apt-get" <<'FAKE'
#!/usr/bin/env bash
echo "apt-get $*" >> "$FAKE_APT_CALLS"
case " $* " in
  *" install "*)
    sleep "${FAKE_APT_SLEEP:-0}"
    echo "Get:1 http://archive.ubuntu.com/ubuntu fake"
    exit "${FAKE_APT_EXIT:-0}"
    ;;
esac
FAKE
chmod +x "$work/apt-get"

failures=0
# run NAME SLEEP EXIT LIMIT EXPECTED_CODE EXPECTED_TEXT
run() {
  local name=$1 sleep=$2 code=$3 limit=$4 expected=$5 text=$6
  local dir="$work/$name"
  mkdir -p "$dir"
  export SYSTEM_PACKAGES_DIR="$dir" SYSTEM_PACKAGES_APT="$work/apt-get"
  export FAKE_APT_CALLS="$dir/calls" FAKE_APT_SLEEP=$sleep FAKE_APT_EXIT=$code
  export GITHUB_STEP_SUMMARY="$dir/summary"
  bash "$script" install libfake-dev pkg-config
  local got=0
  bash "$script" wait "$limit" > "$dir/out" 2>&1 || got=$?
  if [ "$got" -ne "$expected" ]; then
    echo "not ok - $name: exit $got, expected $expected"; cat "$dir/out"; failures=$((failures + 1)); return
  fi
  if ! grep -qF -- "$text" "$dir/out"; then
    echo "not ok - $name: the output does not say '$text'"; cat "$dir/out"; failures=$((failures + 1)); return
  fi
  if [ "$expected" -ne 0 ] && ! grep -qF -- "$text" "$dir/summary"; then
    echo "not ok - $name: the summary does not say '$text'"; failures=$((failures + 1)); return
  fi
  echo "ok - $name"
}

run "a slow install that ends inside the wait passes" 3 0 20 0 "were installed after"
run "a failed install fails, saying apt failed" 0 100 20 1 "apt-get failed (exit 100)"
run "an install still running when the wait ends fails, saying it timed out" 8 0 2 1 "still running after a wait of 2 s"

# apt's output is shown, and apt is asked to retry and not to hang on a stalled request.
dir="$work/a slow install that ends inside the wait passes"
if grep -qF "Get:1 http://archive.ubuntu.com/ubuntu fake" "$dir/out" \
  && grep -q "Acquire::Retries=5 .*update" "$dir/calls" \
  && grep -q "Acquire::http::Timeout=60 .*install -y libfake-dev pkg-config" "$dir/calls"; then
  echo "ok - apt's output is shown and its retries and timeouts are set"
else
  echo "not ok - apt's output or its options"; cat "$dir/out" "$dir/calls"; failures=$((failures + 1))
fi

# Both workflows install through the script and wait the same, longer than ten minutes.
waits=$(grep -ho 'system-packages.sh wait [0-9]*' "$here/ci.yml" "$here/smoke-linux.yml" | sort -u)
if [ "$(grep -c 'system-packages.sh install' "$here/ci.yml" "$here/smoke-linux.yml" | grep -c ':1$')" -eq 2 ] \
  && [ "$(echo "$waits" | wc -l)" -eq 1 ] && [ "${waits##* }" -gt 600 ]; then
  echo "ok - ci.yml and smoke-linux.yml wait ${waits##* } s for the packages"
else
  echo "not ok - the workflows' installs and waits: $waits"; failures=$((failures + 1))
fi

if [ "$failures" -gt 0 ]; then
  echo "$failures failed"
  exit 1
fi
