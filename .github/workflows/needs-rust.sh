#!/usr/bin/env bash
# Says whether a change needs the Rust checks: prints `false` when none of the paths it changes
# is something the Rust checks read, `true` otherwise. `ci.yml` uses it to let its two Rust jobs
# pass at once on a pull request, or a merge queue entry, that only touches documentation or the
# frontend; the push to `dev` that follows the merge runs them.
#
#   needs-rust.sh                      reads the paths, one per line
#                                      (`git diff --no-renames --name-only`), on standard input.
#   needs-rust.sh --event NAME [BASE]  lists them itself for the GitHub event NAME, in the
#                                      checkout of that event:
#       pull_request  what the merge commit GitHub made changes from `dev` (HEAD^1..HEAD);
#       merge_group   what the merge queue entry changes from BASE, the queue's base commit
#                     (`github.event.merge_group.base_sha`): this pull request and the ones
#                     queued before it, on top of `dev`. BASE is fetched when the shallow
#                     checkout lacks it.
#     Any other event, a missing BASE, or a diff that cannot be made prints `true`: what this
#     script cannot tell runs the checks.
#
# It lives next to the workflows so that `.github/CODEOWNERS` covers it: it decides whether
# the tests run. `needs-rust.test.sh` tests it, and ci.yml's Linux job runs that test. The list
# of what is *not* Rust input is short on purpose, and anything it does not name counts as Rust
# input:
#   - `docs/`, `LICENSE`, the issue templates and the Markdown files at the repository root
#     (a Markdown file inside a crate may be a test fixture);
#   - the frontend, `apps/desktop/`, except `src-tauri/` (the Rust side of the app) and
#     `src/bindings.ts` (generated from Rust, and compared with it by the Linux job).
# Rust reads nothing else from there: its tests use their own fixtures, `schema/` and
# `testing/`. If that changes, change this list in the same pull request.
set -euo pipefail

# Reads paths on standard input and prints the answer.
classify() {
  local any=false path
  while IFS= read -r path || [ -n "$path" ]; do
    path=${path%$'\r'}
    [ -n "$path" ] || continue
    any=true
    case "$path" in
      *..*) ;;
      apps/desktop/src-tauri/* | apps/desktop/src/bindings.ts) ;;
      docs/* | LICENSE | .github/ISSUE_TEMPLATE/* | apps/desktop/*) continue ;;
      */*) ;;
      *.md) continue ;;
      *) ;;
    esac
    echo true
    return 0
  done
  # No path at all is not a change this script understands: run the checks.
  if [ "$any" = true ]; then echo false; else echo true; fi
}

# The paths the change of event `$1` (base `$2`) makes, or nothing and a failure when they
# cannot be told.
changed_paths() {
  local event=$1 base=${2:-}
  case "$event" in
    pull_request)
      git diff --no-renames --name-only HEAD^1 HEAD
      ;;
    merge_group)
      [ -n "$base" ] || return 1
      if ! git cat-file -e "$base^{commit}" 2> /dev/null; then
        git fetch --quiet --no-tags --depth=1 origin "$base" > /dev/null 2>&1 || return 1
      fi
      git diff --no-renames --name-only "$base" HEAD
      ;;
    *)
      return 1
      ;;
  esac
}

if [ "${1:-}" = --event ]; then
  if paths=$(changed_paths "${2:-}" "${3:-}" 2> /dev/null); then
    printf '%s\n' "$paths" | classify
  else
    echo true
  fi
else
  classify
fi
