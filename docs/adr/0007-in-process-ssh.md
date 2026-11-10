# ADR-007: In-process SSH with `russh`

- **Status:** Conditional on phase 0 (experiment 2)
- **Date:** 2026-11-02

## Context
Tunnels as first-class citizens: unlimited ProxyJump, ssh-agent on every platform
(including Pageant and the Windows OpenSSH agent), keys, passphrases, certificates, 2FA,
reconnection and keepalive.

## Decision
SSH inside the process with `russh`:
- Fine control of the lifecycle, several database connections over a single SSH session,
  2FA/passphrase prompts in the UI and precise per-hop errors.
- It does not depend on the installed OpenSSH (on Windows it varies and has no
  `ControlMaster`).

Escape hatch: a hop can be **an external command whose stdin/stdout is the transport**
(equivalent to `ProxyCommand`): `ssh -W %h:%p bastion`, `cloudflared`, `aws ssm`… It covers
FIDO2, Kerberos and exotic bastions without implementing them.

Rules:
- Agent: Unix socket (`SSH_AUTH_SOCK`), the `\\.\pipe\openssh-ssh-agent` named pipe and
  Pageant.
- `~/.ssh/config`: a subset is imported (`Host`, `HostName`, `User`, `Port`,
  `IdentityFile`, `IdentityAgent`, `ProxyJump`, `CertificateFile`,
  `ServerAliveInterval`); anything unsupported is reported, never silently ignored.
- known_hosts: the user's + our own; fingerprint confirmation the first time; a changed
  fingerprint is a blocking error. In the headless MCP, unknown host = rejection.
- A shell is never requested from the bastion: only `direct-tcpip`.
- The driver connects to an ephemeral local port on `127.0.0.1` (works with any driver and
  with Postgres cancellation, which opens another connection).
- Reconnection with explicit states and backoff. **Nothing is ever re-run automatically
  after reconnecting.**

## Phase 0 evidence (2026-11-02, `spikes/ssh-hops`)
- `russh` 0.65 (with `ring`; `aws-lc-rs`, the default backend, needs CMake and NASM on
  Windows) reaches PostgreSQL through 2 hops with a key, a key with a passphrase and
  Pageant. PostgreSQL cancellation works through the tunnel.
- It ships Pageant, the OpenSSH named pipe, certificates, keyboard-interactive and GSSAPI.
- Pending: the Windows OpenSSH agent (service disabled on the test machine) and the agent
  via `SSH_AUTH_SOCK` on Linux.
- Finding: even with an agent, the key must be pinnable per hop (`IdentityFile` +
  `IdentitiesOnly`); trying all the agent's keys hits `MaxAuthTries`.
- Measured on Windows 11 against the Docker bastions: 2 hops with a key in 43 ms + 75 ms;
  a wrong passphrase gives a clear error; Pageant 0.83 through both hops; `pg_sleep(30)`
  cancelled through the tunnel after 1.0 s; 1 M rows streamed through both hops in
  1.5–1.7 s (~600,000 rows/s, debug build), the first row after 45 ms.
- Note (2026-11-10): the experiment's code was removed from the repository after v0.8.0.
  It can be read at tag `v0.8.0`, under `spikes/`.

## Implementation (2026-11-02, `crates/spelunk-ssh`)
- Independent of the configuration: it receives an already resolved `TunnelSpec` (the core
  translates from `spelunk-config`). Dialogs (unknown host, passphrase, password, 2FA) go
  through the `Interaction` trait; `NonInteractive` (for the MCP) rejects everything.
- Methods: agent (on Windows, OpenSSH and then Pageant), key (with a `-cert.pub`
  certificate if present and the passphrase asked up to three times), password and
  `keyboard-interactive`. With `identity_file`, the agent only tries that key.
- known_hosts: the user's is read and new keys are learned in our own; a changed key =
  `HostKeyChanged` with file and line, with no possible prompt. Host authorities
  (`@cert-authority`) are not supported yet.
- Ephemeral local port on `127.0.0.1`; several connections per tunnel. If a session drops,
  the tunnel goes `Down` and stops listening; it is never reopened on its own.
- A hop through an external command only on the first hop (it is like `ProxyCommand`).
- Found by the tests: on Windows, `cmd /C` needs the command without re-escaping
  (`raw_arg`); with Rust's escaping, a quoted path broke. It also affected the `cmd:`
  secrets of `spelunk-config`.
- **RUSTSEC-2023-0071 (Marvin, `rsa` crate) is ignored in `cargo-deny`, deliberately:** it
  is a timing side channel in RSA private-key operations. In an SSH client the private key
  signs once per connection and whoever could measure it is the server we authenticate
  to; with an agent the signature does not even happen in this process. There is no fixed
  version and removing RSA would break very common user keys. Revisit when a fixed `rsa`
  exists.
- TLS through the tunnel: the driver connects to the local end (`connect_addr`) but
  verifies the certificate against the server's real name, like `hostaddr` in libpq.
  Tested end to end with two bastions and `verify-full`.
- Pending: automatic reconnection (v0.2) and chaos tests (killing a hop).
- Keep-alives (2026-11-05): every session sends an SSH keep-alive after 15 s without hearing
  from its server and is closed as down after 3 unanswered, so an idle tunnel is neither
  dropped by a NAT nor left looking alive after the network went away. Each tunnel file can
  change both (`keepalive_interval`, `keepalive_count`, like OpenSSH's `ServerAliveInterval`
  and `ServerAliveCountMax`; 0 seconds sends none). Tested with a relay that stops passing
  bytes without closing anything, as a firewall that forgot the session does.
- Connect timeout (2026-11-05): each hop has 15 s to answer and finish the SSH handshake
  (before, only the first hop's TCP connection and handshake had a limit; a later hop or a
  `command` could wait forever). `connect_timeout` in the tunnel file changes it.
  Authentication is not counted: it may be waiting for a passphrase or a 2FA code.
- Local ports (2026-11-05): a connection can fix the port its forward listens on
  (`local_port`, for other tools to use the same tunnel), and the personal `user.toml` can
  limit tunnels to a range (`[tunnels] port_min`/`port_max`, for firewalls). Forwards are still
  only on 127.0.0.1. They apply to SSH tunnels; Kubernetes forwards still take any free port.

## Alternatives
Delegating to the system `ssh` binary (the VS Code Remote-SSH model).

## Revisit if
Pageant or certificates do not work in `russh` without a reasonable fix.
