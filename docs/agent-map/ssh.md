# Ssh

- `crates/spelaion-ssh`: tunnels with `russh` (ADR-007). It does not know the configuration:
  it receives a `TunnelSpec`. `KnownHosts` checks the teams' `.spelaion/known_hosts` first
  (ADR-022; the resolver passes each only when trusted and unchanged: a folder's connection its
  folder's, a personal one every open folder's, ADR-075), all of them: a key any of them pins
  differently blocks, and one pinning the presented key is enough only when none differs
  (`hostkeys::verify`, `a_key_any_open_folder_pins_differently_blocks`). Then the user's and
  SpelaionDB's, where the first that names the host decides. Each hop
  asks its server for the key types pinned for it first (`hostkeys::pinned_algorithms`, as
  OpenSSH does), so a file that pins only an ECDSA or RSA key still verifies the server; the
  check never changes. A tunnel's `TunnelState` is opening, up, down (with its reason) or
  closed, changed only through its `open`, `lose` and `close` (the first reason wins; closing
  keeps a lost tunnel's reason); SSH and Kubernetes tunnels share it, and every reader matches
  it exhaustively. A hop or SSM session lost while a later hop opens fails the opening with
  its own reason (`Lifecycle::unless_lost` in `tunnel.rs`). A hop's loss is reported when its
  `HopHandler` is dropped (its session ended), since russh skips `disconnected` after a
  connection reset. Dialogs go through the
  `Interaction` trait. A key file is OpenSSH, PEM or PuTTY's `.ppk` (versions 2 and 3, read by
  russh): `auth.rs` asks for its passphrase (three attempts) when russh says it is encrypted
  or, for a `.ppk`, when its `Encryption` header does, since russh reports that case with an
  error of its own (SDB-528); the agent filter reads a `.ppk`'s public key from its unencrypted
  public lines. The `.ppk` test keys are committed in `tests/fixtures/ppk` (made with puttygen;
  `gen-keys.sh` lets their public keys into the bastions). Tests against the
  Docker bastions with `SPELAION_TEST_SSH=127.0.0.1:2221`. A tunnel may go through a
  SOCKS5 or HTTP proxy (`proxy.rs`, ADR-027): its first hop, or the database itself with no
  hops; tests against `testing/docker/proxy` with `SPELAION_TEST_SOCKS=127.0.0.1:11080` and
  `SPELAION_TEST_HTTP_PROXY=127.0.0.1:13128`.
  AWS SSM (ADR-050) is a tunnel file's `[ssm]` table: `ssm.rs` runs `aws ssm start-session` with
  an argument list (values checked, options as `--name=value`) and stops the CLI and its plugin
  together (their process group on Unix, a job object on Windows), a close waiting until both
  have exited; `tunnel.rs` uses it like the proxy. Its tests run the stand-in
  `tests/support/fake_aws.rs` (the `spelaion-test-fake-aws` binary); a real AWS account is only
  tested by hand.
  Kubernetes (ADR-015) is a connection's `[kubernetes]` section, or a tunnel file's (a
  Kubernetes network profile, ADR-050: the resolver merges the two, each setting from one
  place, into the connection's `ResolvedKubernetes`):
  `spelaion-core::kube` runs `kubectl port-forward` and lists contexts, namespaces, names and
  ports for the form's Kubernetes tab; tests against a kind cluster with
  `SPELAION_TEST_K8S=<kubeconfig>` (see `testing/k8s`). Its state rules (opening until the
  first kubectl listens; one kubectl ending stops the others) are also tested with no cluster,
  through the stand-in `crates/spelaion-core/tests/support/fake_kubectl.rs` (the
  `spelaion-test-fake-kubectl` binary).
