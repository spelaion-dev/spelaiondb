# ADR-032: The connection test says which step failed, why, and what to try

- **Status:** Accepted (implemented)
- **Date:** 2026-11-07
- **Builds on:** ADR-007 (in-process SSH), ADR-015 (Kubernetes tunnels), ADR-022 (shared host
  keys), ADR-027 (proxies and network profiles)

## Context
SSH tunnels are SpelaionDB's headline feature, and a tunnel that fails with one raw error line
is the worst first impression it can give. "Test connection" opened the whole chain at once
(through the shared tunnel pool) and showed the first error's text: whether hop 2 refused the
key, bastion 1 was down or the database name was wrong, the person had to read a library's
message and guess. DBeaver and Tabularis both show a step-by-step log for this.

## Decision
1. **The core runs the test step by step and owns every word of it.**
   `spelaion-core::test_connection` (and `test_network_profile` for a network profile alone)
   runs these steps, in order, each `Pending → Running → Ok | Failed | Stopped | Skipped`:
   - read the configuration and secrets (a resolve error is this step's failure);
   - the proxy, when the network profile has one;
   - each SSH hop (connect, host key, authentication);
   - the Kubernetes port-forward, for a connection that uses one;
   - reach the database's address (from the last hop, through the proxy alone, or directly
     with a TCP connection), sending it nothing;
   - log in to the database (or open its file), unless the test is "network only".

   `spelaion-ssh` reports each part of the chain as it starts (`Tunnel::connect_observed`
   with a `TunnelStage`), and an error belongs to the part that was running. It also gained
   `Tunnel::probe`, which opens a channel to the target from the last hop and closes it.
   The steps are sent to an observer every time one changes, and the test ends with a report.
2. **Failures are classified in the core**, into: configuration, password required, cancelled,
   proxy authentication, SSH authentication, host unreachable, host key changed, host key not
   accepted, database authentication, database not found, TLS, timeout, Kubernetes, other.
   SSH and proxy failures come typed from `spelaion-ssh` (`SshError::Timeout`,
   `SshError::Target`, `ProxyFailure`); database failures by the engine's code (SQLSTATE 28 and
   3D000, MySQL 1044/1045/1049, SQL Server 18456/4060), and by message only for the TLS, socket
   and timeout errors the libraries give no code for. Each kind has one hint. **A changed host
   key stays a blocking error**: its hint explains what it may mean and where the old line is,
   and never offers to accept the new key (a unit test holds the hint to that).
3. **Secrets never leave the core in the log or the diagnostics.** Every label, message and the
   diagnostics text go through one `Redactor` before they reach the observer or the report:
   - the secrets it knows: the resolved database, hop and proxy passwords (whatever their
     reference: keychain, `env:`, `op://`, `cmd:`), the passwords typed in the form, and every
     answer given to a dialog during the test (passphrases, passwords, 2FA codes), which a
     wrapper around `Interaction` records;
   - anything shaped like a credential: a URL's `user:password@`, `Basic`/`Bearer` tokens, and
     the value after names ending in `password`, `pwd`, `pass`, `passphrase`, `secret`,
     `token`, `api_key`… followed by `=` or `:`.

   "Copy diagnostics" copies the report's `diagnostics` text, which the core builds from the
   redacted steps and redacts once more; the app never builds that text itself.
4. **A test is its own**: it opens a fresh tunnel (never one from the pool that open tabs share),
   uses any free local port, and closes everything it opened when it ends. **Stop** drops the
   running step (a half-open SSH chain closes its sessions, a hop command is killed) and then
   closes what was opened; nothing is retried or reopened on its own. A dialog the stopped step
   was waiting on is closed in the app (`InteractionClosed`).
5. **A connection has its own connect timeout**, for the database: `connect_timeout = <seconds>`
   in the connection file (1 to 600, 15 when absent, database servers only), covering the TCP
   connection, TLS and the login. It reaches the drivers through `ConnectParams::connect_timeout`
   and `open_connection` holds the whole connect to it too (tokio-postgres only times the
   socket). It is not the tunnel file's `connect_timeout`, which is per SSH hop and part of the
   network profile; the form says so next to each.
6. **The app renders.** The connection form has "Test connection" and "Test network only", the
   network profile editor tests its profile, and both show the log with a line per step, its
   time, the error, the hint (translated in the app by kind), Stop while it runs and "Copy
   diagnostics" when it ends.

## Alternatives
- **Keep one error message and improve the wording per library.** Cheaper, but it cannot say
  which hop failed when the library does not, and a wrong database name and a refused login
  through two bastions would still look alike.
- **Let the app classify from the message text.** Rejected: classification and redaction are
  security-relevant and must be the same for the app, the CLI and the MCP (ADR-002).
- **Test through the shared tunnel pool.** It would reuse a working tunnel and report hops as
  fine without testing them, and a fixed local port would collide with an open tab.

## What would change this
- If people need the test to keep the tunnel for the connection that follows, the pool should
  adopt the test's tunnel instead of closing it.
- If the MCP or the CLI get a `test` command, they should print the same report.
- If redaction by shape removes too much from real messages, its key list should shrink, never
  the known-secret replacement.
