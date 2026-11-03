# ADR-015: Kubernetes connections through `kubectl port-forward`

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
Many databases live inside a Kubernetes cluster with no address reachable from the user's
machine: the usual way in is `kubectl port-forward`. Users already have `kubectl` and a
kubeconfig that knows how to authenticate against their clusters (certificates, tokens,
and exec plugins for EKS, GKE, AKS, OIDC…). SpelunkDB should reach those databases as
easily as the ones behind SSH bastions.

## Decision
- **Kubernetes belongs to the connection, not to a tunnel file.** A connection has an
  optional `[kubernetes]` section: a kubeconfig context (default: the current one), a
  namespace (default: the context's) and a resource: `svc/<name>`, `pod/<name>` or
  `deployment/<name>`; optionally a kubeconfig file and the `kubectl` executable (default:
  `kubectl` on the PATH). The resource is specific to one database, so a named, shared file
  (what SSH tunnels are, because many databases sit behind the same bastions) adds nothing.
- **A connection goes through an SSH tunnel or Kubernetes, never both** (a configuration
  error). SQLite has neither.
- **The connection's port is the resource's port** (a service's port or a container port).
  The connection's host is not used to route; it may be left out and still names the server
  for TLS (default `localhost`).
- **The core runs `kubectl port-forward --address 127.0.0.1 <resource> :<port>`** (no shell,
  arguments as a list), reads the local port `kubectl` chose and gives it to the driver, like
  the local end of an SSH tunnel. One process per resource and port, shared by every
  connection that needs it (`TunnelPool`, ADR-004), stopped with the last one.
- **Same rules as SSH tunnels (ADR-007):** if `kubectl` exits (the pod restarted, the
  credentials expired) the tunnel is `Down` with `kubectl`'s own message; it is never
  restarted on its own and nothing is re-run.
- **Trust:** a kubeconfig can run programs (exec credential plugins) and the section names a
  program (`kubectl`); repository connections already need a trusted repository (ADR-009).
- **The UI has its own Kubernetes tab** in the connection form, with lists filled by asking
  `kubectl` through the core: contexts, namespaces, the names of the chosen resource type and
  the resource's ports (read from its manifest), which set the connection's port. Lists are
  only asked for while the tab is enabled.

## Alternatives
- **A Kubernetes kind of tunnel file** (the first version of this ADR, never released): it
  made the user create and name a tunnel for each database, inside the SSH options.
- **In process with `kube-rs`:** no `kubectl` needed and finer errors, but a large
  dependency tree, the kubeconfig authentication (exec plugins, OIDC, proxies) to keep up
  with, and port-forward only to pods (resolving a service to a pod is ours to do). It can
  replace the process later behind the same configuration.
- **Telling users to run `kubectl port-forward` themselves:** what they do today; no
  sharing, no state, ports to remember.

## Revisit if
Users of Kubernetes often lack `kubectl` (for example, the MCP on machines without it), or
`kubectl port-forward` proves unreliable for long sessions.
