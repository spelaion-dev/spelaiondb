# ADR-002: Core as a library independent of Tauri

- **Status:** Accepted
- **Date:** 2026-11-02

## Context
The desktop app, the CLI and the MCP server need the same logic. If that logic is spread
across Tauri commands or the frontend, the MCP duplicates it (badly) and changing the
shell forces a rewrite.

## Decision
- `spelunk-core` is a Rust library that **does not depend on Tauri**.
- It exposes typed, serializable operations and events (`OpenConnection`, `Execute`,
  `FetchRows`, `Cancel`, `TunnelStateChanged`…).
- The Tauri app, the CLI and the MCP are thin adapters over that API.
- **Everything security-related lives in the core**, never in the UI: statement splitting,
  read/write classification, permissions, audit and secret resolution.
- Types that cross to the frontend are generated in TypeScript from Rust
  (`tauri-specta` or `ts-rs`).

## Consequences
- The shell is reversible: the same core can run in Electron via `napi-rs` or as a sidecar.
- The temptation to "fix it quickly" in a Tauri command or in React must be resisted.

## Revisit if
Non-negotiable while the MCP and the CLI exist.
