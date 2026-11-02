# ADR-004: No daemon — app + standalone `spelunk mcp`

- **Status:** Accepted
- **Date:** 2026-11-02

## Context
The MCP must be usable from Claude Code in a terminal without opening the app. Sharing
open connections between processes requires a daemon or one process serving the other.

## Decision
- **v0.1:** two binaries from the same workspace: the app and the `spelunk` CLI.
  `spelunk mcp` reads the same configuration and the same keychain and opens its own
  tunnels and connections. Read-only.
- **v0.3:** while the app is open, it also serves MCP over a local channel accessible only
  to the user (named pipe on Windows, Unix socket on macOS/Linux). `spelunk mcp` detects the
  app and forwards requests to it: tunnels are shared and writes are confirmed in an app
  dialog. Without the app, it stays in standalone read-only mode.
- The agent's configuration never changes: `{"command": "spelunk", "args": ["mcp"]}`.

Inside the app:
- A single process.
- One SSH tunnel per target, shared (multiplexed channels, reference counted).
- One database connection per editor tab (session isolation) plus one for metadata per
  connection profile.

## Implementation (2026-11-02)
- `spelunk-ssh::Tunnel` separates the chain of sessions (`connect`) from the forwards
  (`forward(target)`): several targets behind the same bastion share sessions, and the same
  target reuses its local port.
- `spelunk-core::TunnelPool` (one per process) shares tunnels between connections with the
  same layer, name and hops. It keeps weak references: the tunnel closes when its last
  connection closes. A downed tunnel is not reused: another one is opened.
- The app and `spelunk mcp` each have their own pool (no sharing between processes until
  v0.3).

## Consequences
- In v0.1, the MCP cannot use connections that require human intervention (2FA, a
  passphrase without an agent).
- Avoided: autostart on three OSes, daemon/app version mismatches, orphan processes with
  open tunnels, and securing the local channel.

## Alternatives
- **Single process** (MCP only with the app open): simpler, loses the Claude Code case.
- **Shared daemon:** too much operational complexity for the value it brings.

## Revisit if
Real MCP usage becomes mostly confirmed writes: then the "app as server" mode is brought
forward.
