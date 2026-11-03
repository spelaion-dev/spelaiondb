#!/usr/bin/env bash
# Release notes for a tag: the hand-written highlights (`docs/releases/<tag>.md`), then every
# commit since the previous tag, each linked to GitHub, and the compare link. The release
# workflow publishes this; run it locally to preview:
#
#   scripts/release-notes.sh v0.2.0
#
# Needs the full history and the tags (in CI: `fetch-depth: 0`).
set -euo pipefail

tag=${1:?usage: scripts/release-notes.sh vX.Y.Z}
root=$(git rev-parse --show-toplevel)
notes="$root/docs/releases/$tag.md"
test -f "$notes" || { echo "missing docs/releases/$tag.md" >&2; exit 1; }

# https://github.com/owner/repo, from CI or from the origin remote.
if [ -n "${GITHUB_REPOSITORY:-}" ]; then
  repo="${GITHUB_SERVER_URL:-https://github.com}/$GITHUB_REPOSITORY"
else
  repo=$(git remote get-url origin | sed -E 's#^git@github.com:#https://github.com/#; s#\.git$##')
fi

# The previous tag reachable from this one; none for the first release.
previous=$(git describe --tags --abbrev=0 "$tag^" 2>/dev/null || true)
range=${previous:+$previous..}$tag

cat "$notes"
echo
echo "## Everything in this release"
echo
# Oldest first; merge commits and the version bump say nothing new.
git log --no-merges --reverse --format="- %s ([%h]($repo/commit/%H))" "$range" |
  grep -v '^- Release [0-9]' || true
if [ -n "$previous" ]; then
  echo
  echo "**Full changelog:** [$previous...$tag]($repo/compare/$previous...$tag)"
fi
