#!/usr/bin/env bash
# Frees the disk that builds take (SDB-168): deletes cargo target folders nothing is using.
#
#   scripts/clean-target.sh [--now] [--dry-run] [TARGET...]
#
# TARGET defaults to $CARGO_TARGET_DIR, else ./target. Several can be given, for example every
# worktree's: scripts/clean-target.sh .claude/worktrees/*/target
#
# Without --now a folder goes only when nothing in it changed for a day: a session that stopped
# without cleaning up leaves nothing behind for long. With --now it goes at once: run it so when
# a card is done (AGENTS.md).
#
# It never deletes a folder in use: one a running process was started from (a test executable,
# the app), one a running command names (rustc --out-dir, the linker, cargo with it as its
# target dir), or one whose cargo lock is held (a build running in it). Such a folder is kept
# and said so; the script ends with status 1 if it kept any.

set -u

now=false
dry=false
targets=()
for arg in "$@"; do
  case "$arg" in
    --now) now=true ;;
    --dry-run) dry=true ;;
    -h|--help) sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    -*) echo "unknown option: $arg" >&2; exit 2 ;;
    *) targets+=("$arg") ;;
  esac
done
[ ${#targets[@]} -eq 0 ] && targets=("${CARGO_TARGET_DIR:-target}")

windows=false
case "$(uname -s)" in MINGW*|MSYS*|CYGWIN*) windows=true ;; esac

# The folder as an absolute path, in the forms a command line may write it.
absolute() { (cd "$1" 2>/dev/null && pwd -P); }

# Whether a running process uses the folder (see the header).
in_use() {
  local dir="$1"
  if $windows; then
    local win
    win="$(cygpath -w "$dir")"
    # Paths in command lines come with either slash; compare both, ignoring case.
    DIR_BACK="$win" DIR_FWD="${win//\\//}" powershell.exe -NoProfile -NonInteractive -Command '
      $back = $env:DIR_BACK.ToLower(); $fwd = $env:DIR_FWD.ToLower()
      $busy = Get-CimInstance Win32_Process | Where-Object {
        $exe = "$($_.ExecutablePath)".ToLower(); $cmd = "$($_.CommandLine)".ToLower()
        $exe.StartsWith($back) -or $cmd.Contains($back) -or $cmd.Contains($fwd)
      } | Where-Object { $_.ProcessId -ne $PID }
      $lock = Join-Path $env:DIR_BACK "debug\.cargo-lock"
      $locked = $false
      if (Test-Path $lock) {
        try { $f = [System.IO.File]::Open($lock, "Open", "ReadWrite", "None"); $f.Close() } catch { $locked = $true }
      }
      if ($busy -or $locked) { exit 0 } else { exit 1 }
    ' >/dev/null 2>&1
    return $?
  fi
  local pid
  for pid in /proc/[0-9]*; do
    [ "$pid" = "/proc/$$" ] && continue
    if readlink "$pid/exe" 2>/dev/null | grep -qF "$dir/"; then return 0; fi
    if tr '\0' ' ' < "$pid/cmdline" 2>/dev/null | grep -qF "$dir"; then return 0; fi
  done
  local lock="$dir/debug/.cargo-lock"
  if [ -e "$lock" ] && command -v flock >/dev/null && ! flock -n "$lock" true 2>/dev/null; then return 0; fi
  return 1
}

# Whether anything in the folder changed in the last day.
recent() { [ -n "$(find "$1" -newermt '-1 day' -print -quit 2>/dev/null)" ]; }

kept=0
for target in "${targets[@]}"; do
  dir="$(absolute "$target")" || { echo "$target: no such folder"; continue; }
  if [ ! -e "$dir/CACHEDIR.TAG" ] && [ ! -d "$dir/debug" ] && [ ! -d "$dir/release" ]; then
    echo "$dir: not a cargo target folder, left alone"
    kept=1
    continue
  fi
  if in_use "$dir"; then
    echo "$dir: in use by a running build or program, kept"
    kept=1
    continue
  fi
  if ! $now && recent "$dir"; then
    echo "$dir: used in the last day, kept (--now deletes it anyway)"
    continue
  fi
  size="$(du -sh "$dir" 2>/dev/null | cut -f1)"
  if $dry; then
    echo "$dir: would delete $size"
  else
    rm -rf "$dir" && echo "$dir: deleted $size" || { echo "$dir: could not delete everything"; kept=1; }
  fi
done
exit $kept
