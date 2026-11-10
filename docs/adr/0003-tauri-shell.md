# ADR-003: Desktop shell with Tauri 2

- **Status:** Conditional on phase 0 (experiment 1)
- **Date:** 2026-11-02

## Context
The UI will be web-based (React): it is where agents are most reliable and where quality
code editors exist (CodeMirror 6). Native options (GPUI, egui, Iced, Qt) and Flutter are
ruled out for lack of an editor and for the agent's fluency.

## Decision
Tauri 2: small installer, little RAM, a Rust core without intermediate layers, and a
mature bundler, updater and plugins.

## Accepted risks
Tauri uses the system web engine: WebView2 (Windows), WKWebView (macOS) and WebKitGTK
(Linux). WebKitGTK is the slowest and most problematic, and development happens on
Windows, so it is the least tested platform.

## Phase 0 evidence (2026-11-02, `spikes/grid-stream`)
- WebView2 (Windows): 1 M rows scrolling at 164 fps, 0 % slow frames.
- WebKitGTK 2.52 on WSLg **without graphics acceleration** (worst case): 50–55 fps, p95 of
  21 ms per frame, < 1 % slow frames. Provisionally approved.
- Pending: confirm on real Linux with a GPU (including an NVIDIA card).
- The full measurements (frame times, loading, memory) are in ADR-010's evidence.
- Note (2026-11-10): the experiment's code and raw reports were removed from the repository
  after v0.8.0. They can be read at tag `v0.8.0`, under `spikes/`.

## Alternatives
- **Electron** (plan B): identical Chromium on all three platforms; the Rust core is reused
  via `napi-rs`, not rewritten.
- **Wails:** the same web engines as Tauri with a smaller ecosystem.

## Revisit if
- The grid with 1 M rows stutters on WebKitGTK after optimizing it (experiment 1).
- Platform-specific code has to be maintained to work around WebKitGTK bugs.
