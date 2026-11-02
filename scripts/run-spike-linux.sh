#!/usr/bin/env bash
# Experiments 1 and 3 on Linux (WSL or real Linux): builds the spike app in release mode and
# runs it in automatic mode. The report ends up in spikes/results/linux-*.json.
#
# Requirements: scripts/setup-wsl.sh run, and spikes/grid-stream/dist/ built
# (npm run build on Windows or on Linux).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
# Outside /mnt/c: building on the Windows disk from WSL is very slow and would also
# overwrite the Windows target/.
export CARGO_TARGET_DIR="$HOME/.cache/spelunk-spike-target"
# shellcheck disable=SC1091
source "$HOME/.cargo/env"

if [ ! -f "$ROOT/spikes/grid-stream/dist/index.html" ]; then
  echo "Missing spikes/grid-stream/dist: run 'npm run build' in spikes/grid-stream" >&2
  exit 1
fi

cd "$ROOT/spikes"
# custom-protocol: embeds dist/ in the binary instead of looking for the dev server.
cargo build --release -p grid-stream --features tauri/custom-protocol

export SPIKE_AUTORUN=1
export SPIKE_RESULTS_DIR="$ROOT/spikes/results"
if (exec 3<>/dev/tcp/127.0.0.1/15432) 2>/dev/null; then
  export SPIKE_PG="host=127.0.0.1 port=15432 user=spelunk password=spelunk dbname=spelunk"
else
  echo "WARNING: Postgres is not reachable on 127.0.0.1:15432 from Linux; only the synthetic grid is measured." >&2
fi

"$CARGO_TARGET_DIR/release/grid-stream"
ls -t "$SPIKE_RESULTS_DIR"/linux-*.json | head -1
