# Acme Memo: a plugin written by someone else

An example of a plugin that is **not part of SpelaionDB**, written in C# by following
[`docs/plugin-authors.md`](../../../docs/plugin-authors.md) and
[`docs/plugin-protocol.md`](../../../docs/plugin-protocol.md). Its engine, `acme-memo`, keeps
three notes in memory: it connects, lists the schema `main` and its table `notes`, describes
it, and answers any `SELECT` with its rows. A database called `refuse` fails to connect, to
show how a plugin's error reaches the user.

It has no dependencies: frames, JSON-RPC and the Arrow IPC messages of a result of text columns
are written by hand in `Plugin.cs`, about 600 lines, compiled by the C# compiler of the .NET
Framework every Windows has.

## Build

```powershell
pwsh examples/plugins/acme-memo/build.ps1
```

It writes `dist/acme-memo-1.0.0-x86_64-pc-windows-msvc.tar.gz` (the program and its
`spelaion-plugin.toml`) and prints its SHA-256 and size, which a release manifest would give.

## Try it

- In SpelaionDB: Settings › Plugins › Plugins you added › Add from a file…, then add a
  connection of "Acme Memo", which the picker lists under "Added by you, not part of
  SpelaionDB".
- In the tests: `SPELAION_TEST_ACME_MEMO=<the archive> cargo test -p spelaion-test-plugin --test
  theirs another_language` adds it from a file and from an address, connects, browses and runs
  a `SELECT`.

SpelaionDB does not ship, build or support it: it is here so authors have a working plugin in a
language other than Rust to start from.
