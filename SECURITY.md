# Security policy

SpelaionDB holds the keys to people's databases: passwords in the system keychain, SSH tunnels,
and an MCP server that lets AI agents reach the connections a user exposes. We take reports
seriously and thank everyone who makes them.

## Reporting a vulnerability
**Do not open a public issue.** Report it privately through GitHub: the repository's
**Security** tab → **Report a vulnerability**
([direct link](https://github.com/spelaion-dev/spelaiondb/security/advisories/new)). Only the
maintainers see it.

Please include what an attacker can do, the steps to reproduce it and the version (or commit)
you tried. We will acknowledge it within a week, keep you informed while we fix it, and credit
you in the advisory unless you prefer otherwise.

## What is in scope
Anything that breaks one of the promises in [`AGENTS.md`](AGENTS.md) and the ADRs, for example:

- a secret (password, key, token) written to a log, a configuration file, the history or an MCP
  response;
- a way around the MCP's read-only mode or the barriers of agents' writes (ADR-008, ADR-019,
  ADR-020);
- a repository's configuration resolving secrets or running commands without the user's trust,
  or a change to a trusted connection used without being reviewed (ADR-009, ADR-022);
- a changed SSH server key accepted without a blocking error;
- a statement run again by itself after a reconnection.

Problems in third-party libraries are welcome too; we will report them upstream.

## Supported versions
SpelaionDB is a preview: fixes go into the latest release and `dev`. Please check that the
problem still happens there.
