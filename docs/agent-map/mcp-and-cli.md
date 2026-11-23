# Mcp and cli

- `crates/spelaion-mcp` + `apps/cli`: `spelaiondb mcp` and utilities (`connections`, `trust`,
  `audit`, `check`: the team repository's CI check, from `spelaion-config::check`, ADR-022). In `spelaiondb mcp`, stdout is the protocol: no `println!`. Reads and writes
  (`execute_write`, only with `[mcp] writes = "allow"`) use separate sessions. What agents may
  do on a connection is `spelaion-core::agent_access` (`write.rs`): the user's read-only mode
  for every agent (ADR-033, `mcp.read_only` in the store, read on every call, failing closed)
  only takes writes away. Every tool call starts by reading the configuration and the trust
  store again (`Snapshot` in `lib.rs`): a change the user makes applies at the next call of a
  running server, never in the middle of one; a configuration with any problem exposes nothing;
  a cached session is reused only while its connection is exposed and its `Definition` (what it
  is resolved from, secrets aside) is unchanged, and only while it is not lost (the core's
  `Liveness`, `open.rs`: its health, its tunnel, its connection, asked by both `Cached::usable`
  and a call that fails); the call that meets a lost session fails and is never
  retried, and the next call opens a new one. Query rows go out as JSON or TOON (`format.rs`); any change there
  keeps every cell quoted so data cannot add rows or fields. The catalog tools' answers
  (`list_objects`, `search_schema`, `describe_table`) are built in `catalog.rs` (SDB-120): their
  lines stop within `format::MAX_RESPONSE` and end with an "N more" line, comments and defaults
  are cut to one line (`format::one_line`), and the structured content is `{"complete",
  "shown", "omitted", "narrow_with"}`, described in `catalog_limit!` and `docs/guide/mcp.md`.
  Every refusal and failure answers its sentence unchanged (clients may match on it) and, as
  `structuredContent`, `{"code", "retry"}` (SDB-374). The code and `retry` are decided in the
  core (`agent_state.rs`, `AgentFailure`) from the core's error types, never from text; the
  code enum is `spelaion_store::FailureCode`, kept by the audit (`audit.code`, a migration
  and an insert trigger listing the codes). A new code goes in `FailureCode::WRITTEN`, the
  trigger (in a new migration), `failure_codes!` in `lib.rs` (every tool's description; a test
  checks it) and `docs/guide/mcp.md`. A failed write never says `retry: true` unless its
  transaction is known to be rolled back. `not_exposed` covers a connection that does not
  exist too, so a code never reveals a hidden one. A missing or refused plugin's engine has a
  driver that only says why (its error is a `DriverError::Connection`), so `plugin_missing` is
  told from `Engines::plugin_unavailable`, not from the error. "Every agent is read-only" is
  `spelaion_core::all_agents_read_only`, asked by the server and by the app's MCP page.
  `explain_query` names the engines `structured=true` works on from the core
  (`plan::agent_plan_engines`: the plan reader's engines that agents see), filled in at start
  in place of `{engines}` in its description and its `structured` argument (SDB-103); a test
  fails if one is missing. Never write engine names there by hand.
  `spelaion-config::mcp_clients` finds each AI client's file and writes our entry there for the
  app's MCP page (ADR-033): only our entry changes, JSON edited as text at the spans it finds,
  Codex's TOML with `toml_edit`, a backup first, and only the change the user saw (its
  fingerprint); the new file and the backup keep the old file's mode (owner-only when new) and
  a symbolic link stays a link. Tests and the smoke test point it at
  `SPELAION_MCP_CLIENTS_HOME`, never at the user's files; `SPELAION_CLI` names the `spelaiondb` it installs. The installers ship the CLI
  next to the app (`externalBin`, only in `tauri.release.conf.json`).
  Every MCP tool call goes to the store's append-only `audit` table with its tool and its
  process's session; the app's Activity tab reads it through `spelaion-core::audit`, which masks
  secrets (`redact_secrets`) before anything is shown or exported. Clearing (`clear_audit`) is
  the only delete and leaves a record.
