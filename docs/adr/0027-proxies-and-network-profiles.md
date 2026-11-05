# ADR-027: SOCKS and HTTP proxies go in tunnel files, and tunnel files are the network profiles

- **Status:** Proposed (waiting for a decision before it is built)
- **Date:** 2026-11-05
- **Builds on:** ADR-007 (in-process SSH), ADR-009 (configuration and secrets), ADR-022 (trust
  pinned to content)

## Context
DBeaver gives each connection "network handlers": an SSH tunnel, a SOCKS proxy and an HTTP
proxy, which can be chained (SSH through a proxy). Its network profiles
(`PrefPageNetworkProfiles`) are named sets of those handlers that many connections point to.

SpelaionDB already has the SSH part: a tunnel is a file in `.spelaion/tunnels/` (or the
personal directory) that any number of connections name with `tunnel = "…"`. It holds the hops
and, since this change, the keep-alive and connect timeout settings. Connections reach the
database through a local forward on 127.0.0.1 (ADR-007), so every driver works through a
tunnel without knowing about it, PostgreSQL cancel included, and TLS is still checked against
the real server name. Today the only way to use a proxy is a hop `command` such as
`ncat --proxy proxy:1080 --proxy-type socks5 %h %p`. That works, but it needs another program
and puts the proxy password in a command line.

Two questions have to be answered first:
1. Where does a proxy live: in each connection, or in the tunnel file?
2. Is a network profile a new kind of file, or is it what a tunnel file already is?

## Decision (recommended)
1. **A proxy is part of a tunnel file**, as an optional `[proxy]` table:
   ```toml
   schema_version = 1

   [proxy]
   type = "socks5"            # or "http" (CONNECT)
   host = "proxy.corp.example"
   port = 1080
   user = "${user.proxy_user}"
   password = { secret = "keychain" }

   [[hops]]                   # optional: with no hops, the proxy alone reaches the database
   host = "bastion.example.com"
   user = "ops"
   ```
   - With hops, the first hop's TCP connection goes through the proxy (SSH through a proxy,
     as DBeaver chains them). Without hops, each connection accepted on the local forward is
     carried through the proxy to the database. Both keep the local forward, so drivers,
     PostgreSQL cancel, TLS name checks, `local_port`, the port range and the tunnel pool
     work unchanged.
   - SOCKS5 resolves the target name on the proxy side (like `socks5h`), so internal names
     that only the proxy knows work. HTTP proxies use `CONNECT host:port`. SOCKS4 and TLS to
     the proxy itself (an `https://` proxy) are left out until someone needs them.
   - The proxy password is a secret reference like any other (ADR-009): `keychain` stores it
     under `…/tunnel/<name>/proxy/password`, never in the file, a log or an MCP answer.
     Changing the proxy's host forgets the stored password, as a hop's does.
   - Repository trust (ADR-022) pins the `[proxy]` table with the rest of the file: a pull
     that points the proxy elsewhere is shown and must be accepted before anything is sent.
   - The code goes in `spelaion-ssh` (it already owns the forward and the first hop's
     connection), with `tokio-socks` (MIT) for SOCKS5 and a small CONNECT client written here.
     The tunnel editor gets a "Proxy" section; the DBeaver import maps its `socks_proxy` and
     `http_proxy` handlers.
2. **Tunnel files are the network profiles.** A tunnel file already is a named, reusable set of
   network settings that many connections share, stored in the repository or the personal
   directory and pinned by trust. Instead of a second kind of file, the app shows them as
   "network profiles": the forms say so, and Settings → Network lists them with New, Edit and
   Delete, so they can be managed without opening a connection. The file name and the
   `tunnel = "…"` key stay as they are, so no existing configuration changes.

## Alternatives
- **A `[proxy]` section in each connection (DBeaver's model).** Easy to explain, but the same
  proxy is repeated in every connection, and a proxy that is not part of the tunnel needs
  either its own local forward (a second copy of the forwarding code) or support in every
  driver (`mysql_async`, `tiberius` and our PostgreSQL driver would each need a custom stream).
  Changing a proxy would mean editing every connection that uses it.
- **A new `profiles/` file type** that bundles a tunnel, a proxy and other settings, with
  connections writing `profile = "…"` and maybe overriding some fields. It copies DBeaver
  closely, but it adds a second concept for what a tunnel file already does, rules for which
  value wins, another set of trust snapshots and a migration for existing tunnels.
- **Keep the `command` workaround only.** No code, but it needs `ncat` or similar on every
  machine, does not work in the MCP on a machine without it, and the password ends up in a
  command line.

## What would change this
- If people need different proxies for connections that share the same SSH hops, the proxy
  belongs to the connection, not the tunnel file.
- If a proxy needs authentication that the driver itself must do (NTLM or Negotiate on a
  corporate HTTP proxy), or proxies that change per network (PAC files), the `command` hop is
  the answer, not built-in proxies.
- If profiles should carry non-network settings shared by many connections (TLS mode,
  read-only, environment) or allow per-connection overrides, a real profile file type pays
  for itself and should replace part 2.
