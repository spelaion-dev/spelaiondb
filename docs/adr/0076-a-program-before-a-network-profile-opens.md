# ADR-076: A network profile may run one program before its route opens: an argument list, waited for, pinned by trust; nothing runs on disconnect and a connection file never names a program for it

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Builds on:** ADR-007 (a hop through an external command), ADR-009 and ADR-022 (repository
  trust, pinned to content), ADR-015 and ADR-050 (`kubectl`, `aws ssm`), ADR-030 (pasted
  connections), ADR-032 (the connection test and its redactor), ADR-037 (what opening a session
  runs), ADR-049 (programs started with an argument list)
- **Amends:** ADR-027 (a network profile may also say what to run before its route opens)
- **Decides:** SDB-42

## Context
DBeaver runs a shell command before and after connecting and disconnecting, per connection,
with options: wait for it, a timeout, show its output, end it at disconnect. People use it to
bring a VPN up, to log in to a cloud (`aws sso login`) and to start a forwarder. It is program
execution driven by configuration, and a repository's connections are written by other people.

What the app already runs, checked on `dev`:

- **Two things run a line of shell from configuration.** A `cmd:` secret (`secrets.rs`,
  `run_shell`: `cmd /C` or `sh -c`) with no time limit, whose error text shows the command's
  own standard error unredacted; and a tunnel's first-hop `command` (`tunnel.rs`,
  `shell_command`, with `%h` and `%p` replaced as text), bounded by the hop's connect timeout
  and killed with its tunnel, but only the shell itself, not what it started.
- **Three run an argument list**, never a shell: `kubectl` (`kube.rs`), `aws ssm` (`ssm.rs`,
  every value checked, the whole process tree stopped through a process group or a job object)
  and the backup tools (ADR-049).
- **Trust covers all of it by construction.** A connection's or tunnel's snapshot is every
  serialized field (`connection_snapshot`, `tunnel_snapshot`), so a new key is pinned without
  listing it; a changed or new item resolves nothing, reads no secret and runs no command until
  it is reviewed (`resolve.rs`, with `tests/config.rs`'s
  `every_way_around_the_snapshot_is_caught`). But the first "trust this repository" dialog
  lists nothing: it pins whatever the files hold, programs included.
- **A pasted connection** brings nothing that runs a program (`copy.rs`'s `sanitize`), and
  keeps its `tunnel` only when a tunnel of that name already exists here.
- **Agents open connections through the same `open_connection`** as the app
  (`spelaion-mcp`'s `session_for`), so `cmd:` secrets and hop commands already run when an
  agent uses an exposed connection.
- **There is no one place where "disconnected" happens.** A session per tab, a pooled tunnel
  that closes with its last session (`TunnelPool`), sessions dropped rather than closed when
  something still holds them, an app exit that does not drop its state, and a crash that runs
  nothing.
- **The DBeaver import** neither brings its shell commands nor lists them as left out.

So the risk is not new: a trusted repository can already make the app run a command when
connecting. What is missing is a declared, bounded place for it, so that nobody has to hide one
inside a password's `cmd:`.

## Decision
1. **One thing: a program that runs before a network profile's route opens.** A tunnel file
   (a network profile, ADR-027) may have a `[before]` table:
   ```toml
   [before]
   program = "tailscale"
   args = ["up", "--accept-routes"]
   timeout = 60        # seconds; 30 when absent, at most 600
   ```
   It may stand with the profile's hops, proxy, SSM or Kubernetes settings, or alone: a
   profile that only prepares a direct route. A connection uses it as it uses any profile,
   with `tunnel = "<name>"`.
2. **It belongs to the profile, never to a connection file.** A connection file gets no key
   for it. So the text of a copied or pasted connection (ADR-030) has no place for a program,
   and a paste keeps its `tunnel` only when a profile of that name already exists here: what
   would run is the user's own file, or a repository's that went through trust. Importing
   from another client never writes a `[before]`:
   the client's shell commands are listed as not imported (ADR-039), DBeaver's included, which
   are not listed today. A VPN is also shared by the connections behind it, which is what a
   profile is.
3. **An argument list, never a shell.** `program` is one program and `args` its arguments,
   each its own string, started as ADR-049 starts the backup tools: no `sh -c`, no `cmd /C`,
   no console window on Windows, no standard input. Only `${user.…}` variables are expanded,
   each inside its own argument; nothing of a connection (host, user, database) and no secret
   of ours is put into the arguments or the environment, which is the app's own. A user who
   wants a shell writes `program = "sh"` and its `-c` themselves, where the review shows it.
4. **It is waited for, with a time limit.** The route opens only after the program exits with
   0. Any other exit, or the time limit, fails the opening with the step's name, the exit code
   and what the program wrote; at the limit, and when the user cancels, the program and what it
   started are stopped together (as `ssm.rs` does). There is no "do not wait".
5. **It runs when the route opens, not when a session does.** Once per opening of the
   profile's route in `TunnelPool`, whoever opens it: a session, the backup tools' route
   (`open_route`) or the connection test, which opens its own route and so runs it too. It does
   not run again while the route is in use. It runs again when the route is opened again, which
   only happens on a user's click (ADR-028): it is the profile's, like the session setup is the
   connection's (ADR-037), and nothing the user ran is run again. It must therefore be safe to
   run twice, and the form says so.
6. **Its output is shown, bounded and redacted, and kept nowhere.** Standard output and error,
   up to 16 KiB each, go through the connection test's `Redactor` (ADR-032) to the test's step
   and to the error of an opening that failed. They are never written to the app's log, the
   history or the audit log. **An agent never gets them**: when the route is opened for the MCP
   server, a failure says which program failed and its exit code, and no more.
7. **Trust (ADR-022).** `[before]` is part of the tunnel file's snapshot, so a pull that adds
   it or changes its program, an argument or its time limit is shown and must be accepted
   before the profile resolves, for every connection that uses it, personal ones included.
   - **A repository profile's program cannot be a file inside the repository**: its content
     would not be pinned, and a pull could change what runs without changing the profile. It
     is a name found on the `PATH`, an absolute path or one under the home directory; the core
     resolves it before starting it, never from the current directory.
   - **The first trust dialog names every profile with a `[before]`**, with its program and
     arguments, before the user trusts the repository.
   - The editors that test a profile check it the same way before a process starts
     (`check_tunnel_editor_access`).
   - Tests try to get around each of these, next to the ones that exist for hop commands.
8. **`spelaiondb check`** fails on a `[before]` whose program is inside the repository or whose
   arguments seem to hold a password (the heuristic it has for `cmd:`), and warns when a
   repository profile's program is a shell or an interpreter.
9. **Agents.** The MCP server opens routes through the same code, so the program runs for an
   agent's session as hop commands and `cmd:` secrets do, with the same limits and with point
   6's silence. An agent cannot choose or change it: it comes from files the user wrote or
   reviewed, read again on every call.
10. **Nothing runs after connecting, or before or after disconnecting, and nothing is kept
    running.** After connecting, what a session needs is SQL, which is ADR-037's. A program at
    disconnect could not be promised (see the context), would make quitting wait, and "VPN
    down" takes the network from every other program using it. A program kept running for the
    life of the route is a forwarder, which needs a port, a way to know it is ready and a state
    of its own (`TunnelState`): it is a kind of network profile, as SSM is (ADR-050), and a
    decision of its own.
11. **The file that starts the program is listed in `.github/CODEOWNERS`**, with the model's
    load rules for `[before]`.

## Alternatives
- **No** (the most reasonable alternative): people start their VPN themselves. It adds nothing
  to attack or maintain. But the door is already open in a worse shape: a `cmd:` that brings
  the VPN up before printing the password is a line of shell with no time limit, in a field
  nobody reads as a command.
- **DBeaver's four events, on the connection, as a shell line.** A connection has many
  sessions and one route, so "connect" and "disconnect" are not events of a connection here;
  the disconnect ones cannot be guaranteed; a shell line is quoted differently by `cmd` and
  `sh` and invites values pasted into it; and a connection file is what copies, pastes and
  imports carry.
- **Personal profiles only**, never a repository's. The strictest, and a team connection could
  still name a profile each person writes for themselves. But a repository profile can already
  carry a hop `command` under the same review, so this would refuse the bounded form and keep
  the unbounded one; and a team could not share "bring the VPN up, then the bastion".
- **Not for agents.** The MCP server would refuse, or skip, a profile with `[before]`.
  Skipping would send the connection down a path its profile did not intend; refusing would
  take agents away from every connection behind a VPN. The program is not the agent's to
  choose, and hop commands already run there.

## What would change this
- If reviews of repository changes turn out to be accepted without reading, every key that
  names a program (this one, a hop's `command`, `cmd:`) moves to the personal layer, in one
  ADR.
- If people need a forwarder started and stopped with the route (Cloud SQL Auth Proxy, IAP,
  Teleport), a forwarder profile, decided on its own.
- If a program must run at disconnect for a reason other than tidiness, that case is brought
  back with what it needs to be reliable.

## Work items if accepted
- `[before]` in network profiles: the model and its load rules, the start in the route's
  opening with the time limit and the process tree, the test's step and redaction, trust tests
  that try to get around it, the tunnel form, `spelaiondb check` (M; an owner's review).
- The first trust dialog lists the programs a repository's files can start: `[before]`, hop
  commands, `cmd:` secrets, `kubectl`, `aws` and `sqlite3` paths (S).
- The DBeaver import lists its shell commands as not imported (S).

Found on the way, for the Backlog, not part of this decision: a `cmd:` secret has no time
limit, and a hop `command` and `kubectl` stop only the process they started, not its children.
