# ADR-050: Network profiles reach Kubernetes clusters and AWS SSM managed nodes

- **Status:** Accepted
- **Date:** 2026-11-11
- **Amends:** ADR-015 (Kubernetes belongs to the connection), ADR-027 (tunnel files are the
  network profiles)
- **Builds on:** ADR-022 (trust pinned to content), ADR-049 (running the user's programs)

## Context
"First-class tunnels" is SpelaionDB's headline, and two common ways in are missing:

- **Kubernetes users have several databases behind one cluster.** ADR-015 put Kubernetes in
  each connection's `[kubernetes]` section, because the resource is specific to one database.
  But the context, the namespace, the kubeconfig and the `kubectl` to use are the same for every
  database of a cluster, and each connection repeats them. A team that moves its kubeconfig, or
  a user whose `kubectl` is not on the PATH, edits every connection.
- **AWS shops increasingly use SSM Session Manager instead of bastions.** `aws ssm
  start-session` with the `AWS-StartPortForwardingSession` documents forwards a local port to a
  port on a managed node, or through it to another host (RDS, ElastiCache), with the user's AWS
  credentials and no SSH server or open inbound port. Today the only way is a hop `command`
  that runs `aws ssm start-session --document-name AWS-StartSSHSession`, which needs an SSH
  server on the node and cannot reach RDS without one more hop.

ADR-027 already made tunnel files the network profiles: named, reusable, in the repository or
the personal directory, pinned by trust. Both cases fit there.

## Decision
1. **A network profile may be a Kubernetes one:** a `[kubernetes]` table in a tunnel file, with
   the same settings as a connection's `[kubernetes]` section (ADR-015):
   ```toml
   schema_version = 1

   [kubernetes]
   context = "prod-eu"
   namespace = "data"
   kubeconfig = "${user.kubeconfig}"   # optional; KUBECONFIG or ~/.kube/config otherwise
   kubectl = "${user.kubectl}"         # optional; kubectl on the PATH otherwise
   # resource = "svc/orders"           # optional; otherwise each connection names its own
   ```
   - A connection uses it with `tunnel = "<profile>"`, like any profile, and its own
     `[kubernetes]` section sets what the profile leaves out (usually only `resource`). **A
     setting comes from one place:** set in both is an error, so there is no rule for which
     one wins. The resource must come from one of them.
   - A Kubernetes profile has no hops, proxy or SSH settings (keep-alives, connect timeout);
     a connection through it has no `local_port` (kubectl chooses its own) and cannot be a
     local file. A connection's `[kubernetes]` with an SSH profile is still refused: a
     connection goes through SSH or Kubernetes, never both (ADR-015).
   - The profile's values are expanded where the profile lives, and a relative `kubeconfig` is
     resolved against the profile's layer (the repository root for a team profile).
   - What runs does not change: `kubectl port-forward` started by the core with an argument
     list, one process per resolved forward (cluster, namespace, resource and port) shared by
     every connection that needs it, never restarted on its own (ADR-015). Two connections that
     resolve to the same forward share it, whichever profile they name.
   - **Trust:** the table is part of the tunnel file's snapshot (ADR-022), so a pull that
     changes the `kubectl`, the kubeconfig, the context, the namespace or the resource, or turns
     an SSH profile into a Kubernetes one, is shown and accepted before anything resolves. The
     editors that list contexts, namespaces and resources with `kubectl` check the profile the
     same way before a process starts: an unreviewed repository profile runs nothing.
   - **Custom `kubectl` and kubeconfig paths** are settings of the profile (or of the
     connection, as before). A personal value per machine goes in a `${user.…}` variable, so a
     shared profile names no local path.
   - The UI: the network profile editor has a route choice (SSH hops and proxy, or
     Kubernetes); the Kubernetes tab of the connection form picks a Kubernetes profile, shows
     what it sets locked, and lists the rest with `kubectl` as before. The SSH tab offers only
     SSH profiles. Settings → Network shows a Kubernetes profile's route as
     `context / namespace / resource`.
2. **A network profile may start with AWS SSM:** an `[ssm]` table in a tunnel file:
   ```toml
   schema_version = 1

   [ssm]
   target = "i-0123456789abcdef0"   # the managed node (an instance or mi-… id)
   region = "eu-west-1"             # optional; the AWS CLI's default otherwise
   profile = "prod"                 # optional; the AWS CLI's default otherwise
   aws = "${user.aws}"              # optional; aws on the PATH (or its installer's folder)
   ```
   - **Without hops**, SSM reaches the database: the connection's host and port, as seen from
     the managed node. `localhost` (or `127.0.0.1`, `::1`) is a port on the node itself
     (`AWS-StartPortForwardingSession`); any other host is reached through the node
     (`AWS-StartPortForwardingSessionToRemoteHost`), which is how RDS and ElastiCache are
     reached. The connection's host still names the server for TLS.
   - **With hops**, SSM reaches the first hop's SSH port and the hops go on from there (SSH
     over SSM, without `AWS-StartSSHSession` or a `command`). A proxy, a first hop `command`
     and Kubernetes are refused together with SSM.
   - **How it runs:** the core starts `aws ssm start-session --target=… --document-name=…
     --parameters=…` (plus `--region=…` and `--profile=…` when set) as an argument list, never
     through a shell, with a local port it chose on 127.0.0.1, and waits for the Session
     Manager plugin to say it is waiting for connections. Each value is checked before it
     becomes an argument (a target, region or host with a character that could change the
     command or the `--parameters` list is refused), and every option is written as
     `--name=value`, so no value can become an option. One process per target, shared by the
     tunnel's connections; it is stopped with the tunnel, and the plugin that `aws` starts is
     stopped with it (the whole process tree). If it exits, the tunnel is down with the AWS
     CLI's own message, and nothing is restarted or re-run (ADR-007).
   - **Nothing secret goes to it.** SSM uses the user's AWS credentials as the AWS CLI finds
     them (profiles, SSO, environment); the profile file holds no secret, and no database or
     SSH password is ever an argument, an environment variable of the process, or a log line.
   - **Detected, explained:** before starting, the core looks for `aws` (the named one, the
     PATH, then the AWS CLI installer's folders) and for `session-manager-plugin` (the PATH,
     its installer's folders, and next to `aws`). What is missing is said by name, with what
     to install; a plugin found outside the PATH is put on the process's PATH.
   - **Trust:** the `[ssm]` table is pinned with the rest of the file (ADR-022); `aws` names a
     program, so a repository profile is used only as the user trusted it, like a hop
     `command` or `kubectl`.
   - The UI: the network profile editor's route choice adds "AWS SSM", with the target, region,
     profile and AWS CLI path, and the hops after it.

## Implementation
- Part 1 (Kubernetes profiles, 2026-11-11): `KubernetesProfileConfig` in `TunnelFile`; the
  resolver (`resolve.rs`) merges it with the connection's `[kubernetes]` into
  `ResolvedConnection::kubernetes`, so the core's `kubectl port-forward` is unchanged.
  `Resolver::kubernetes_profile_for_editor` and `check_tunnel_editor_access` guard the editors'
  `kubectl` lists. Tested in `spelaion-config/tests/network_profiles.rs` (trust included) and
  against the kind cluster of `testing/k8s` (`spelaion-core/tests/kube.rs`, the smoke test).
- Part 2 (SSM) follows in its own change.

## Alternatives
- **Kubernetes profiles as a new file type** (`kubernetes/<name>.toml`): a second kind of
  profile, its own trust snapshots and its own list in the UI, for what a tunnel file already
  is. ADR-027 rejected the same for proxies.
- **Profile values as defaults the connection may override:** easy to write, hard to read: a
  connection that sets `kubectl` would silently differ from its teammates', and a pasted
  connection could replace a reviewed profile's program. One place per setting is simpler and
  safer.
- **A personal default `kubectl` and kubeconfig in Settings:** useful only when a single
  cluster tool is used everywhere; a `${user.…}` variable in the profile does the same per
  profile and needs no new setting. Revisit if users ask.
- **SSM in process, with the AWS SDK and our own Session Manager protocol:** no AWS CLI needed,
  but a large dependency tree, the AWS credential chain (SSO, MFA, `credential_process`) to keep
  up with, and an undocumented WebSocket protocol to implement. The CLI and its plugin are what
  AWS supports.
- **Only the `AWS-StartSSHSession` command hop:** works today for SSH over SSM, but needs an SSH
  server on the node and cannot reach RDS directly.

## Consequences
- SSM needs the AWS CLI v2 and the Session Manager plugin on the user's machine, and the MCP
  needs them where it runs. The error says which is missing.
- SSM is tested with stand-ins for `aws` and the plugin that record their arguments and
  environment and forward the port; a real AWS account is tested by hand.

## Revisit if
- Users of SSM often lack the AWS CLI (the MCP on servers): then an in-process client.
- Teams need a profile's settings to differ per connection (another namespace per database
  with everything else shared): then per-field overrides with a visible rule, not silent ones.
