#!/usr/bin/env bash
# Prepares Ubuntu or Debian (WSL or real Linux) to build SpelaionDB: Tauri 2 dependencies
# (WebKitGTK 4.1) and stable Rust. Node.js, for the frontend in apps/desktop, is not
# installed here.
#
# Other distributions: this script only knows apt. It prints the packages Tauri's
# prerequisites page gives for Arch and Fedora and stops; for those and any other
# distribution, see https://v2.tauri.app/start/prerequisites/#linux and install Rust with
# rustup (or the distribution's own rust package). The building notes, including how much
# memory a build needs, are in CONTRIBUTING.md.
set -euo pipefail

if ! command -v apt-get >/dev/null 2>&1; then
  cat >&2 <<'EOF'
This script installs packages with apt-get, which this system does not have.

Arch Linux:
  sudo pacman -S --needed webkit2gtk-4.1 base-devel curl wget file openssl \
    appmenu-gtk-module libappindicator-gtk3 librsvg xdotool

Fedora:
  sudo dnf install webkit2gtk4.1-devel openssl-devel curl wget file \
    libappindicator-gtk3-devel librsvg2-devel libxdo-devel
  sudo dnf group install "c-development"

Other distributions: https://v2.tauri.app/start/prerequisites/#linux
Then install Rust (https://rustup.rs) and Node.js, and see CONTRIBUTING.md.
EOF
  exit 1
fi

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
