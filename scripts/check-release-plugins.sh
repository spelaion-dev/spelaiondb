#!/usr/bin/env bash
# Checks that release builds carry the engine plugins `plugins.toml` pins (ADR-042): each
# pinned plugin for the target exactly once, next to `spelaiondb`, with the pinned program
# SHA-256. The release workflow's rehearsal runs it on every installer and CLI archive it
# builds; run it locally on files a rehearsal uploaded:
#
#   scripts/check-release-plugins.sh x86_64-unknown-linux-gnu SpelaionDB_0.10.0_amd64.deb
#
# Each argument is a folder (an unpacked `.app`, say) or a `.deb`, `.AppImage`, `.tar.gz`,
# `.zip`, `.msi` or NSIS `-setup.exe`, unpacked into a temporary folder first. `.msi` needs
# Windows (`msiexec`), `.zip` and `.exe` need 7-Zip, `.deb` needs `dpkg-deb`, `.AppImage` the
# runner's own architecture.
set -euo pipefail

usage="usage: scripts/check-release-plugins.sh <target triple> <installer, archive or folder>..."
target=${1:?$usage}
shift
[ $# -gt 0 ] || { echo "$usage" >&2; exit 2; }

root=$(cd "$(dirname "$0")/.." && pwd)
ext=""
case "$target" in *-windows-*) ext=".exe" ;; esac

# "name program_sha256" for every plugin pinned for this target. A `[[...]]` header starts a
# new entry (`[[withdrawn]]` entries have names but never a program hash).
expected=$(awk -v want="$target" '
  /^\[\[/ { name = ""; platform = "" }
  /^name = / { v = $3; gsub(/"/, "", v); name = v }
  /^\[plugin\.platform\./ { p = $1; sub(/^\[plugin\.platform\./, "", p); sub(/\]$/, "", p); platform = p }
  /^program_sha256 = / && platform == want && name != "" { v = $3; gsub(/"/, "", v); print name, v }
' "$root/plugins.toml")
[ -n "$expected" ] || { echo "plugins.toml pins no plugin for $target" >&2; exit 1; }

sha256() {
  if command -v sha256sum > /dev/null; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}
# Native Windows programs (7-Zip, msiexec) want Windows paths.
native() {
  if command -v cygpath > /dev/null; then cygpath -w "$1"; else printf '%s\n' "$1"; fi
}
absolute() { printf '%s/%s\n' "$(cd "$(dirname "$1")" && pwd)" "$(basename "$1")"; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
failed=0
fail() { echo "FAIL $1" >&2; failed=1; }

n=0
for input in "$@"; do
  n=$((n + 1))
  dir="$work/$n"
  mkdir -p "$dir"
  if [ -d "$input" ]; then
    dir=$input
  else
    [ -f "$input" ] || { fail "$input: no such file"; continue; }
    file=$(absolute "$input")
    case "$file" in
      *.deb) dpkg-deb -x "$file" "$dir" ;;
      *.AppImage) chmod +x "$file"; (cd "$dir" && "$file" --appimage-extract > /dev/null) ;;
      *.tar.gz) tar xzf "$file" -C "$dir" ;;
      *.zip | *.exe) 7z x -y "-o$(native "$dir")" "$(native "$file")" > /dev/null ;;
      *.msi)
        # An administrative install only unpacks the files, and Start-Process waits for it.
        powershell.exe -NoProfile -Command "\$p = Start-Process msiexec -Wait -PassThru -ArgumentList '/a','$(native "$file")','/qn','TARGETDIR=$(native "$dir")'; exit \$p.ExitCode"
        ;;
      *) fail "$input: not a folder, .deb, .AppImage, .tar.gz, .zip, .msi or .exe"; continue ;;
    esac
  fi

  cli=$(find "$dir" -type f -name "spelaiondb$ext")
  if [ "$(printf '%s' "$cli" | grep -c . || true)" -ne 1 ]; then
    fail "$input: expected one spelaiondb$ext, found: ${cli:-none}"
    continue
  fi
  home=$(dirname "$cli")
  while read -r name pinned; do
    found=$(find "$dir" -type f -name "$name$ext")
    count=$(printf '%s' "$found" | grep -c . || true)
    if [ "$count" -ne 1 ]; then
      fail "$input: expected one $name$ext, found $count"
    elif [ "$(dirname "$found")" != "$home" ]; then
      fail "$input: $name$ext is not next to spelaiondb$ext (${found#"$dir"/})"
    elif [ "$(sha256 "$found")" != "$pinned" ]; then
      fail "$input: $name$ext has SHA-256 $(sha256 "$found"), plugins.toml pins $pinned"
    else
      echo "ok   $input: $name$ext next to spelaiondb$ext, SHA-256 $pinned"
    fi
  done <<< "$expected"
done

exit "$failed"
