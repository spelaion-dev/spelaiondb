# Ssh

- `crates/spelaion-ssh`: tunnels with `russh` (ADR-007). It does not know the configuration:
  it receives a `TunnelSpec`. `KnownHosts` checks the team's `.spelaion/known_hosts` first
  (ADR-022; the resolver passes it only when trusted and unchanged), then the user's. Each hop
  asks its server for the key types pinned for it first (`hostkeys::pinned_algorithms`, as
  OpenSSH does), so a file that pins only an ECDSA or RSA key still verifies the server; the
  check never changes. Dialogs go through the `Interaction` trait. Tests against the
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
  `SPELAION_TEST_K8S=<kubeconfig>` (see `testing/k8s`).
