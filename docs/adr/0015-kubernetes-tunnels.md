# ADR-015: Kubernetes tunnels through `kubectl port-forward`

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
Many databases live inside a Kubernetes cluster with no address reachable from the user's
machine: the usual way in is `kubectl port-forward`. Users already have `kubectl` and a
kubeconfig that knows how to authenticate against their clusters (certificates, tokens,
and exec plugins for EKS, GKE, AKS, OIDC…). SpelunkDB should reach those databases the way
it reaches the ones behind SSH bastions: a tunnel the connection refers to by name.

## Decision
- **A tunnel is SSH or Kubernetes.** A Kubernetes tunnel names a kubeconfig context
  (default: the current one), a namespace (default: the context's) and a resource:
  `svc/<name>`, `pod/<name>` or `deployment/<name>`; optionally a kubeconfig file and the
  `kubectl` executable (default: `kubectl` on the PATH).
- **The connection's port is the remote port**, as with SSH, where the connection's host
  and port are the target seen from the last hop. The connection's host is not used to route
  (it still names the server for TLS).
- **The core runs `kubectl port-forward --address 127.0.0.1 <resource> :<port>`** (no shell,
  arguments as a list), reads the local port `kubectl` chose and gives it to the driver, like
  the local end of an SSH tunnel. One process per resource and port, shared by every
  connection that needs it (`TunnelPool`, ADR-004), stopped with the last one.
- **Same rules as SSH tunnels (ADR-007):** if `kubectl` exits (the pod restarted, the
  credentials expired) the tunnel is `Down` with `kubectl`'s own message; it is never
  restarted on its own and nothing is re-run.
- **Trust:** a kubeconfig can run programs (exec credential plugins) and the tunnel names a
  program (`kubectl`), so repository Kubernetes tunnels need a trusted repository (ADR-009),
  like hops through an external command. Personal ones are the user's own.
- The UI fills contexts, namespaces and resources by asking `kubectl` (only when the user
  opens the form), and "Test" checks that the resource exists with `kubectl get`.

## Alternatives
- **In process with `kube-rs`:** no `kubectl` needed and finer errors, but a large
  dependency tree, the kubeconfig authentication (exec plugins, OIDC, proxies) to keep up
  with, and port-forward only to pods (resolving a service to a pod is ours to do). It can
  replace the process later behind the same tunnel kind.
- **Telling users to run `kubectl port-forward` themselves:** what they do today; no
  sharing, no state, ports to remember.

## Revisit if
Users of Kubernetes often lack `kubectl` (for example, the MCP on machines without it), or
`kubectl port-forward` proves unreliable for long sessions.
