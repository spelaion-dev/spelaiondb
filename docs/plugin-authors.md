# Writing a plugin for SpelaionDB

This guide is for people who want SpelaionDB to reach an engine it does not have, by writing a
plugin of their own. `docs/plugin-protocol.md` is the contract the plugin speaks; this guide is
what to build around it, how to hand it to users, and what SpelaionDB does with it.

**Your plugin is not part of SpelaionDB, and the project does not support it** (ADR-041, point
5; ADR-055, point 5). It is never in SpelaionDB's plugin catalogue, which lists only the
plugins SpelaionDB builds and tests. A user adds yours themselves, from a file or from an
address they type, after a dialog that tells them what it is. From then on:

- **Agents get nothing on its engine.** Its connections are never shown to an MCP client,
  whatever they say, and `spelaiondb mcp` never starts your program.
- **There is no read-only mode on its engine**, not even a weaker one: SpelaionDB can only
  promise read-only connections on engines whose barrier it wrote and tests (ADR-020).
- **It is labelled as not ours** on the Plugins page, in the "add connection" picker and on
  its connections, and every error it gives is shown with its name, its version and its author,
  saying that it is not part of SpelaionDB. Reports about it go to you.
- **No updates are offered.** A new version is a new file or address the user gives, with the
  dialog again.
- **It is pinned by hash.** SpelaionDB records your program's SHA-256 when the user adds it
  and checks it before every start: a program that changed (updated itself, was replaced) is
  refused until the user trusts it again.

Becoming one of SpelaionDB's own engines (its repository under `spelaion-dev`, built by our CI,
its dialect and barriers in the core with their tests) is the only way into the catalogue, and is
decided one plugin at a time.

## What you ship

For each platform, **one `.tar.gz` archive** holding, at its top, by their own names:

- `spelaion-plugin.toml`, the manifest (below);
- **your program: one file**, which SpelaionDB starts with no arguments and talks to over its
  standard input and output. On Windows it ends in `.exe`;
- optionally a few other plain files (a licence, a readme): never unpacked.

No folders, links or other kinds of entries: an archive that has them is refused. At most 16
files, at most 256 MiB for the archive, and a program of at most 512 MiB.

**Only the program is unpacked, and only its hash is pinned.** A program that loads other files
next to it (scripts, `.jar`s, libraries) would load files nobody pinned, and they are not there:
build one self-contained executable. Ways that work: a compiled language (Rust, Go, C, C++, C#,
Swift…), a single-file .NET publish, GraalVM's `native-image` for Java, PyInstaller's `--onefile`
for Python, Node's single executable applications. A Rust plugin can use `spelaion-plugin-sdk`,
which implements the protocol for you.

The platforms are SpelaionDB's own: `x86_64-pc-windows-msvc`, `aarch64-apple-darwin`,
`x86_64-apple-darwin` and `x86_64-unknown-linux-gnu`.

## The manifest

`spelaion-plugin.toml`, in each archive:

```toml
schema_version = 1
name = "acme-memo"                 # your plugin's name
version = "1.0.0"                  # three numbers
author = "Acme Ltd."               # who you are, as users will read it
description = "Notes kept in memory, to show how a plugin is written."   # optional
homepage = "https://acme.example/memo"    # optional, https://; shown as text, never opened
protocol = "1.0"                   # the plugin protocol version it speaks (docs/plugin-protocol.md)
platform = "x86_64-pc-windows-msvc"       # the platform this archive is for
program = "acme-memo-plugin.exe"   # the program at the archive's top
engine = "acme-memo"               # the engine it adds, as connections name it
engine_label = "Acme Memo"         # the engine's name as people write it
dialect = "postgres"               # optional: one of SpelaionDB's dialects (see below)
```

The rules, checked when the user adds it (anything else refuses the whole manifest):

- `name` and `engine`: lowercase letters, digits and `-`, starting with a letter (`engine` up
  to 40 characters). Neither may start with `spelaion`, and `engine` may not be one of
  SpelaionDB's own engines (`postgres`, `duckdb`…) or of the server kinds of its picker
  (`cockroachdb`, `valkey`…): a plugin that is not ours never passes for one that is.
- **One plugin per engine.** A plugin whose engine another plugin the user added already serves
  is refused until that one is removed. A newer version of the same plugin (same `name`)
  replaces the old one, after the dialog.
- `protocol`: the major version SpelaionDB speaks and a minor not newer than its own. Within a
  major version, what you do not use you may ignore (see "Versions" in the protocol).
- `platform` must be the computer's.
- Unknown fields are refused, so a manifest cannot claim anything the rules here do not read:
  there is no field for agents, read-only mode or a barrier of your own.

### The dialect

SpelaionDB splits every script into statements itself, before your plugin sees it, and quotes
the names it writes (opening a table, the explorer's actions). `dialect` says how it reads your
engine's language:

- **None:** standard SQL's lexical rules: `'…'` strings and `"…"` names with doubled quotes,
  `--` and `/* */` comments.
- **One of SpelaionDB's** (an engine name: `postgres`, `mysql`, `sqlserver`, `sqlite`, `duckdb`,
  `clickhouse`, `trino`, `oracle`, `cassandra`, `mongodb`…): that engine's splitter and quoting,
  for an engine that speaks a dialect close to it (PostgreSQL's `$$` bodies, MySQL's backticks).

Either way your engine's scripts are read **under the name `external`**: SpelaionDB borrows the
reading and nothing else. No barrier, server family or SQL that SpelaionDB keeps for the named
engine applies to yours.

## Handing it to users

**From a file:** users choose your archive on the Plugins page ("Add from a file…").

**From an address:** users type or paste an address into the Plugins page, and nowhere else
(no link, file association, command-line argument, repository file or connection file can give
SpelaionDB an address). It is read once, when they ask:

- It is the `https://` address of your archive, or of a **release manifest** naming an archive
  for each platform. Every redirect must stay on `https://`; an address with a user name or a
  password in it is refused, and so is a host written with characters outside ASCII (give the
  `xn--` form).
- A release manifest is a small TOML file (at most 64 KiB):

  ```toml
  schema_version = 1
  name = "acme-memo"
  version = "1.0.0"

  [platform.x86_64-pc-windows-msvc]
  archive = "https://acme.example/memo/acme-memo-1.0.0-x86_64-pc-windows-msvc.tar.gz"
  archive_sha256 = "…"             # lowercase hexadecimal
  archive_size = 123456            # bytes, at most 256 MiB

  [platform.x86_64-unknown-linux-gnu]
  archive = "https://acme.example/memo/acme-memo-1.0.0-x86_64-unknown-linux-gnu.tar.gz"
  archive_sha256 = "…"
  archive_size = 123456
  ```

  SpelaionDB fetches the archive for the user's platform (exactly `archive_size` bytes, with
  `archive_sha256`), and the archive's own manifest must give the same `name` and `version`.

The dialog shows the user the whole address, its host, the archive's address and host when they
differ, where the bytes finally came from after a redirect, the SHA-256 of what arrived and of
your program, and what your manifest says. Nothing in the archive is run or unpacked before the
user says yes. Publish your hashes somewhere users can compare them.

A connection that uses your engine names the engine and nothing else (`engine = "external"`,
`external_engine = "acme-memo"`): it can be shared in a team's repository, and a teammate who has
not added your plugin is told that the connection needs an engine that is not part of SpelaionDB
and none of their plugins serves; nothing is offered or downloaded for them.

## Speaking the protocol

Everything is in `docs/plugin-protocol.md`; this is the least a plugin needs for a user to
connect, browse and run statements:

1. **Frames** on standard input and output: a 4-byte little-endian length, a kind byte (`1` a
   JSON-RPC message, `2` data), the payload. **Write nothing else to standard output**; write
   your logs to standard error, one line at a time (SpelaionDB logs them after taking out the
   secrets it gave you).
2. **`initialize`**: answer with the version you chose among those offered, your name and
   version, the engines you serve (your manifest's `engine`) and your `credit`.
3. **`connect`**: the session id, host, port, user, `password`, database, TLS mode and
   `connect_addr` (the local end of an SSH tunnel SpelaionDB opened: connect there, verify TLS
   against `host`). Answer with your `capabilities`, every field present (`false` for what you
   do not do). Keep the password in memory only; never write it anywhere.
4. **`close`** ends a session; **`cancel`** interrupts what a session runs.
5. **Browsing:** `server_version`, `list_schemas`, `list_objects`, `describe_table`, and
   `list_columns` for the editor's completion. Answer each with JSON in the field names of
   `spelaion-driver-api` (the protocol's table). Answer what you do not support with an error
   of kind `other`, and an unknown method with JSON-RPC's `-32601`: never with silence.
6. **`execute`** runs one statement: answer `{}` at once, then send its result on the stream:
   a data frame with flag `1` carrying an Arrow IPC schema message, data frames with record
   batches (no more than your `credit` before the core gives credit back with `stream.credit`),
   then `stream.done` (or `stream.error`) and always `stream.end`. **Rows never travel as JSON.**
   Each Arrow field carries the metadata `spelaion.logical` (`text`, `int`, `float`, `bool`,
   `timestamp`…, whose Arrow type it must have) and `spelaion.native_type` (your engine's own
   name for the type). Sending every column as `text` (Arrow `Utf8`) with its native type is
   always accepted.
7. **Exit when standard input closes**, after closing what you hold open.

Errors carry `data` with a `kind` (`connection`, `database`, `cancelled`, `decode`, `other`)
and a `message`. SpelaionDB puts your plugin's name, version and author before every message
it shows, so do not repeat them, and never put a secret in one.

What you must never do is in the protocol's last section: decide whether a statement is a read
(you run what you are sent), re-run anything after a lost connection, outlive standard input,
or read the password from anywhere but `connect`.

## Trying it

1. Build your program and pack the archive (`tar -czf acme-memo.tar.gz spelaion-plugin.toml
   acme-memo-plugin.exe` works on Windows, macOS and Linux).
2. In SpelaionDB, Settings › Plugins › "Plugins you added" › "Add from a file…", read the
   dialog, and add it.
3. Add a connection: your engine is in the picker under "Added by you, not part of SpelaionDB".
   Test it: the connection test's diagnostics name your plugin.
4. After rebuilding, add the new archive the same way (same `name`, a new `version` if you like):
   it replaces the old one. A program you replace by hand in the user's folder is refused until
   they trust it again.

`examples/plugins/acme-memo/` is a complete plugin written this way, in C# with no
dependencies: an engine that keeps a few notes in memory, which connects, browses and answers
`SELECT`s. Its `build.ps1` compiles it with the C# compiler every Windows has and packs the
archive.

## What SpelaionDB decides, not you

SpelaionDB is the one that splits and classifies statements, holds passwords and tunnels, asks
the user before risky runs and audits. Your plugin is the wire to your engine: it connects,
runs what it is sent, cancels and describes what is there. If users ask for read-only mode or
agents on plugins like yours, whether a barrier a plugin declares can ever be vouched for needs a
decision of its own (an ADR); until then the answer is no.
