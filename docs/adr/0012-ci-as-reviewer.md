# ADR-012: CI is the code reviewer

- **Status:** Accepted
- **Date:** 2026-11-02

## Context
There is no human code review. Automated checks are the only filter.

## Decision
- **Mandatory gates:** `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo-deny`
  (licenses and vulnerabilities), strict TypeScript + ESLint, all tests green.
- **Real integration** in Docker: PostgreSQL (several versions), MySQL 8, MariaDB;
  in-memory SQLite.
- **Type zoo:** per engine, a table with every type and snapshot tests (`insta`) of
  display and of the round trip when editing.
- **Multi-hop bastion** in Docker (`testing/docker`): chained networks, authentication
  variants (key, certificate, password + TOTP with a fixed seed), a bastion without a
  shell, restricted `PermitOpen`, a real agent, and chaos tests (killing a hop).
- GitHub Actions Windows runners do not run Linux containers: integration only on Linux;
  on Windows/macOS, unit tests + a smoke test with SQLite. Pageant: manual checklist.
- **UI:** Playwright against the frontend with a mocked core; a few tests with
  `tauri-driver`.
- **Signing:** macOS with Apple Developer + notarization; Windows via SignPath Foundation
  (OSS). v0.1 unsigned.
- **Updates:** `tauri-plugin-updater` with signed manifests on GitHub Releases; the key in
  a CI secret + an offline copy.
