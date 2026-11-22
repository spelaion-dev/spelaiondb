#!/usr/bin/env bash
# Tests needs-rust.sh (SDB-81): which paths count as Rust input, and how it lists them itself
# for a pull request and for a merge queue entry, in a throwaway repository shaped like those
# checkouts. Whatever it cannot tell must answer `true`. ci.yml's Linux job runs it first;
# locally: `bash .github/workflows/needs-rust.test.sh`.
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
script="$here/needs-rust.sh"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

failures=0
expect() {
  local name=$1 want=$2 got=$3
  if [ "$got" = "$want" ]; then
    echo "ok: $name"
  else
    echo "FAIL: $name: expected $want, got $got"
    failures=$((failures + 1))
  fi
}

# Paths on standard input.
paths() { printf '%s\n' "$@" | bash "$script"; }
expect "documentation only" false "$(paths docs/guide/mcp.md README.md LICENSE)"
expect "the frontend only" false "$(paths apps/desktop/src/App.tsx apps/desktop/package.json)"
expect "an issue template" false "$(paths .github/ISSUE_TEMPLATE/bug.yml)"
expect "a crate" true "$(paths docs/a.md crates/spelaion-core/src/lib.rs)"
expect "the Rust side of the app" true "$(paths apps/desktop/src-tauri/src/mcp.rs)"
expect "the generated bindings" true "$(paths apps/desktop/src/bindings.ts)"
expect "a Markdown file inside a crate" true "$(paths crates/spelaion-core/tests/fixture.md)"
expect "a workflow" true "$(paths .github/workflows/ci.yml)"
expect "a path that climbs" true "$(paths docs/../Cargo.toml)"
expect "a Windows line ending" false "$(printf 'docs/a.md\r\n' | bash "$script")"
expect "no path at all" true "$(printf '' | bash "$script")"

# A repository: `dev` with a commit, then a branch for each kind of change.
repo="$work/repo"
git init --quiet --initial-branch=dev "$repo"
cd "$repo"
git config user.email test@example.invalid
git config user.name test
git config commit.gpgsign false
git config core.autocrlf false
# As GitHub does: a commit can be fetched by its id while a branch reaches it.
git config uploadpack.allowReachableSHA1InWant true
mkdir -p docs crates/core
echo "# docs" > docs/a.md
echo "fn main() {}" > crates/core/main.rs
git add -A
git commit --quiet -m base
base=$(git rev-parse HEAD)

commit_on() {
  local branch=$1 file=$2
  git checkout --quiet -b "$branch" "$base"
  mkdir -p "$(dirname "$file")"
  echo "change" >> "$file"
  git add -A
  git commit --quiet -m "$branch"
}
commit_on only-docs docs/b.md
commit_on rust crates/core/main.rs
commit_on frontend apps/desktop/src/App.tsx

# pull_request: GitHub checks out a merge commit whose first parent is `dev`.
event() { bash "$script" --event "$@"; }
pull_request() {
  git checkout --quiet --detach "$base"
  git merge --quiet --no-ff --no-edit "$1"
  event pull_request
}
expect "pull_request, documentation" false "$(pull_request only-docs)"
expect "pull_request, Rust" true "$(pull_request rust)"
expect "pull_request, frontend" false "$(pull_request frontend)"

# merge_group: the queue's branch is the queued changes on top of its base commit; the base
# is given, and fetched when the checkout lacks it.
group() {
  git checkout --quiet --detach "$base"
  local branch
  for branch in "$@"; do git cherry-pick --quiet "$branch" > /dev/null; done
}
group only-docs
expect "merge_group, documentation" false "$(event merge_group "$base")"
group frontend only-docs
expect "merge_group, frontend and documentation" false "$(event merge_group "$base")"
# A documentation change queued behind one that changes Rust is tested with it.
group rust only-docs
expect "merge_group, documentation behind Rust" true "$(event merge_group "$base")"
group only-docs
expect "merge_group without its base" true "$(event merge_group "")"
expect "merge_group with a base that cannot be fetched" true \
  "$(event merge_group 0123456789abcdef0123456789abcdef01234567)"

# A shallow clone that lacks the base fetches it from origin.
git checkout --quiet only-docs
shallow="$work/shallow"
git clone --quiet --depth=1 --branch only-docs "file://$repo" "$shallow"
cd "$shallow"
expect "merge_group, base fetched into a shallow checkout" false "$(event merge_group "$base")"
cd "$repo"

# Anything else runs the checks.
expect "push" true "$(event push)"
expect "workflow_dispatch" true "$(event workflow_dispatch)"
expect "no event" true "$(event "")"

# Both jobs ask it the same way, for pull requests and merge queue entries alike.
ci="$here/ci.yml"
asks=$(grep -c 'bash .github/workflows/needs-rust.sh --event "$GITHUB_EVENT_NAME" "$MERGE_GROUP_BASE"' "$ci" || true)
expect "both Rust jobs ask needs-rust.sh with the event" 2 "$asks"
bases=$(grep -c 'MERGE_GROUP_BASE: ${{ github.event.merge_group.base_sha }}' "$ci" || true)
expect "both Rust jobs pass the queue's base" 2 "$bases"
grep -qE '^  merge_group:' "$ci" && queued=yes || queued=no
expect "ci.yml runs for the merge queue" yes "$queued"

if [ "$failures" -gt 0 ]; then
  echo "$failures failure(s)"
  exit 1
fi
echo "needs-rust.sh: all checks passed"
