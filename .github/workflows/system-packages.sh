#!/usr/bin/env bash
# Installs the Ubuntu packages a Linux job needs while the job does other work, then waits for
# them (SDB-427). ci.yml's Linux job and smoke-linux.yml both use it:
#
#   bash .github/workflows/system-packages.sh install PACKAGE...   # returns at once
#   bash .github/workflows/system-packages.sh wait SECONDS         # before the first build
#
# `install` runs `apt-get update` and `apt-get install` in the background, with apt's own
# retries and timeouts: a request to the mirror that fails or stalls is made again instead of
# failing or hanging the install. When it ends it writes `ready` or `failed <exit code>` to
# `apt.status`. `wait` waits for that file for up to SECONDS, shows apt's output, and passes
# only on `ready`. It fails in two ways and says which one, in the log and in the job's summary:
# apt failed (an error, not a wait), or apt was still running when the wait ended (a slow
# mirror). A slow mirror once took more than ten minutes, so the workflows wait twenty.
#
# It lives next to the workflows so that `.github/CODEOWNERS` covers it, and needs-rust.sh
# counts it as something the Rust checks read. `system-packages.test.sh` tests it with a fake
# apt-get (SYSTEM_PACKAGES_APT and SYSTEM_PACKAGES_DIR are only for that test).
set -euo pipefail

dir=${SYSTEM_PACKAGES_DIR:-${RUNNER_TEMP:?RUNNER_TEMP is not set}}
read -ra apt <<< "${SYSTEM_PACKAGES_APT:-sudo apt-get}"
# Each request is tried up to six times; one that sends nothing for a minute is a failed try.
options=(-o Acquire::Retries=5 -o Acquire::http::Timeout=60 -o Acquire::https::Timeout=60)

usage() {
  echo "usage: $0 install PACKAGE... | wait SECONDS" >&2
  exit 2
}

# Says why the packages are not there: in the log, as an error annotation, and in the summary.
fail() {
  echo "::error title=System packages::$1"
  echo "**System packages:** $1" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"
  exit 1
}

case "${1:-}" in
  install)
    shift
    [ "$#" -gt 0 ] || usage
    rm -f "$dir/apt.status" "$dir/apt.status.part"
    (
      code=0
      { "${apt[@]}" "${options[@]}" update && "${apt[@]}" "${options[@]}" install -y "$@"; } || code=$?
      if [ "$code" -eq 0 ]; then echo ready; else echo "failed $code"; fi > "$dir/apt.status.part"
      # Whole or not there: `wait` never reads half a line.
      mv "$dir/apt.status.part" "$dir/apt.status"
    ) > "$dir/apt.log" 2>&1 &
    ;;
  wait)
    limit=${2:-}
    [[ "$limit" =~ ^[0-9]+$ ]] || usage
    start=$SECONDS
    until [ -f "$dir/apt.status" ] || [ $((SECONDS - start)) -ge "$limit" ]; do sleep 1; done
    echo "::group::apt-get output"
    cat "$dir/apt.log" 2> /dev/null || true
    echo "::endgroup::"
    status=$(cat "$dir/apt.status" 2> /dev/null || true)
    case "$status" in
      ready)
        echo "The system packages were installed after $((SECONDS - start)) s of waiting."
        ;;
      "failed "*)
        fail "apt-get failed (exit ${status#failed }): an error, not a slow mirror. Its output is in the log of this step."
        ;;
      "")
        fail "apt-get was still running after a wait of $limit s: it had not failed, the Ubuntu mirror was slow. Running the job again usually passes."
        ;;
      *)
        fail "apt-get ended with an unknown status: '$status'."
        ;;
    esac
    ;;
  *)
    usage
    ;;
esac
