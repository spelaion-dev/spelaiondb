#!/usr/bin/env bash
# Reads the paths a pull request changes, one per line (`git diff --no-renames --name-only`),
# and prints `false` when none of them is something the Rust checks read, `true` otherwise.
# `ci.yml` uses it to let its two Rust jobs pass at once on a pull request that only touches
# documentation or the frontend; the push to `dev` that follows the merge runs them.
#
# It lives next to the workflows so that `.github/CODEOWNERS` covers it: it decides whether
# the tests run. The list of what is *not* Rust input is short on purpose, and anything it
# does not name counts as Rust input:
#   - `docs/`, `LICENSE`, the issue templates and the Markdown files at the repository root
#     (a Markdown file inside a crate may be a test fixture);
#   - the frontend, `apps/desktop/`, except `src-tauri/` (the Rust side of the app) and
#     `src/bindings.ts` (generated from Rust, and compared with it by the Linux job).
# Rust reads nothing else from there: its tests use their own fixtures, `schema/` and
# `testing/`. If that changes, change this list in the same pull request.
set -euo pipefail

any=false
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
  exit 0
done

# No path at all is not a pull request this script understands: run the checks.
if [ "$any" = true ]; then echo false; else echo true; fi
