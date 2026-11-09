# ADR-013: License, contributions and repository structure

- **Status:** Accepted
- **Date:** 2026-11-02

## Decision
- **Apache-2.0 license** (patent grant and clear contributions). AGPL adds nothing without
  a server mode; GPL would deter adoption in companies.
- **DCO** (`Signed-off-by`) instead of a CLA.
- **Compatible monetization:** paid features built on top (open core), donations (GitHub
  Sponsors / Open Collective) or paid signed binaries.
- **Trademark policy:** forks cannot be called SpelunkDB. Check that the name is free
  before investing in it.
- **Known legal risk:** the code is written entirely by an AI; in the EU and the US,
  copyright generally requires human authorship. Consult a lawyer if monetization becomes
  serious.
- **Monorepo:** a Cargo workspace + a JS package workspace. (Superseded by ADR-042 for engine
  plugins: each has its own repository.)

```
crates/   spelunk-core, spelunk-config, spelunk-ssh, spelunk-driver-api,
          spelunk-driver-postgres, spelunk-driver-sqlite, spelunk-mcp, spelunk-store
apps/     desktop (Tauri + React), cli (`spelunk` binary)
spikes/   phase 0 experiments (throwaway code)
testing/  docker compose: databases + bastions
docs/     adr/, architecture.md
```

- **No web mode:** it is not designed for one.

## Revisit if
The plan becomes selling commercial licenses of the same code (dual licensing): then
GPL-3.0 + CLA from the first external PR.
