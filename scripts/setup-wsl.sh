#!/usr/bin/env bash
# Prepares Ubuntu (WSL or real Linux) to build SpelaionDB: Tauri 2 dependencies
# (WebKitGTK 4.1) and stable Rust. Node.js, for the frontend in apps/desktop, is not
# installed here.
set -euo pipefail

sudo apt-get update
sudo apt-get install -y \
  build-essential curl wget file pkg-config \
  libwebkit2gtk-4.1-dev libxdo-dev libssl-dev \
  libayatana-appindicator3-dev librsvg2-dev

if ! command -v rustup >/dev/null 2>&1 && [ ! -x "$HOME/.cargo/bin/rustup" ]; then
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
fi

"$HOME/.cargo/bin/rustc" --version
pkg-config --modversion webkit2gtk-4.1
echo "SETUP-WSL DONE"
