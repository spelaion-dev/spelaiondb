# ADR-077: Importing connections does not read the passwords other clients keep under Windows DPAPI, for now; if it ever does, through one small crate of our own

- **Status:** Accepted
- **Date:** 2026-11-24
- **Builds on:** ADR-009 (secrets), ADR-012 (CI is the reviewer), ADR-063 (the Windows
  installers are not signed yet)
- **Amends:** ADR-039 (what it left for later about DPAPI-protected stores is decided here)
- **Decides:** SDB-165

## Context
Three imports stop at the same place on Windows. MySQL Workbench keeps saved passwords in
`workbench_user_data.dat`, DataGrip (and every JetBrains IDE) in its own KeePass database whose
master key is in `c.pwd`, and TablePlus in `.tpmaster`. All three are protected with the
Windows account's key (DPAPI), which any program of that user opens by calling
`CryptUnprotectData`. Today each import brings the connection and its preview says the
password will be asked when connecting (`workbench.rs`, `datagrip.rs`, `tableplus.rs`).
ADR-039 left reading them as "one decision for all of them".

Checked on `dev`:

- **The workspace has no `unsafe` at all.** `[workspace.lints.rust] unsafe_code = "forbid"`,
  and every one of the eighteen crates takes the workspace's lints; no crate has its own.
  `forbid` cannot be lifted for a module. AGENTS.md allows "justified FFI with a comment", but
  so far nothing needed it: the registry is read with `windows-registry`, child processes are
  tied to a job object with `win32job`, the keychain goes through `windows-native-keyring-store`.
- **No crate in the dependency tree wraps DPAPI safely.** The raw binding is there
  (`CryptUnprotectData` in `windows-sys` and `windows`, with the feature other dependencies
  already turn on), and calling it is `unsafe`.
- **Other clients' stored passwords are already read, when they are files a reader can open
  with safe code**: DBeaver's (a key built into DBeaver), Beekeeper's (its `.key`),
  HeidiSQL's (reversible), and the macOS keychain's entries of DataGrip and Workbench, only on
  an import the user asked for with passwords (ADR-039's point 5).
- **Workbench's vault is simple once opened**: one line per entry, the connection's host
  identifier, its user and the password (SDB-105; the fixture's README records the local
  check). It maps onto the `KeychainItem` the macOS import already uses.
- **DataGrip needs more than DPAPI**: a reader of KeePass's format for `c.kdbx`. There is none
  in the tree.
- **TablePlus's `.tpmaster`** has no published format.
- **The Windows installers are unsigned** and stay so for now (ADR-063's note of 2026-11-23).
- **The app collects nothing** (no telemetry), so nobody can see how often an import is
  abandoned for its passwords.

## Decision
1. **Not now.** No import reads a DPAPI-protected store. The connection is imported and the
   password is asked the first time it connects, then kept in SpelaionDB's own keychain entry,
   as today. The previews keep saying so.
2. **Why not, weighed honestly.**
   - **What it would give.** More than "one prompt per connection": someone who has the
     password only in the old client has to find it or have it reset. That is real for DataGrip
     on Windows, where its own KeePass database is the default. It is still once per
     connection, and only for people coming from these three clients on Windows.
   - **It would be the workspace's first `unsafe`, in the code that holds other programs'
     passwords.** Nobody reviews this code line by line; the compiler does (ADR-012). A call
     into the system that hands back a pointer, a length and a buffer to free is exactly what
     the compiler does not check, and a mistake there leaks or corrupts a secret.
   - **Unsigned, it is the wrong moment.** Opening other programs' credential stores is what
     password stealers do. A security product that flags an unsigned SpelaionDB for it would
     stop every Windows user from installing it, not only the few who import. Whether that
     happens cannot be known without building it.
   - **The larger audience costs the most.** Workbench alone is small. DataGrip also needs a
     KeePass reader: a new dependency, or a parser of our own for a format with several
     ciphers.
3. **Consent, if it is ever read:** ADR-039's point 5 as it stands. Only on an import the
   user started with "store the passwords" checked, never while previewing or listing clients;
   the password goes straight to SpelaionDB's keychain entry as a `Secret`. Windows asks the
   user nothing when a program opens that user's DPAPI data (macOS does ask for another app's
   keychain entry), so the dialog's own words are the only consent and must name the store.
4. **How, if it is ever read: one small crate of our own.** Decided now, so that reopening
   this is short:
   - a crate in the workspace, built only for Windows, with one function: protected bytes in,
     a `Secret` out. It calls the binding already in the tree, so no dependency is added. It has
     its own lint table (`unsafe_code = "deny"`, lifted for the one block, with the comment
     AGENTS.md asks for), holds nothing else, and is listed in `.github/CODEOWNERS`;
   - its test protects a value through DPAPI itself and reads it back, on CI's Windows job; a
     vault is never committed, since it opens only for the account that wrote it;
   - **not a third-party wrapper**: none is in the tree, and adding one puts the same `unsafe`
     in a small crate nobody here has read;
   - **not PowerShell's `ProtectedData`**: no `unsafe`, but a child process that prints a
     password into a pipe, and a pattern security products stop sooner than the call itself;
   - Workbench first; DataGrip only once a KeePass reader passes `cargo deny` and an owner's
     review; TablePlus's `.tpmaster` not at all, as long as its format is not published.
5. **SDB-105 (Workbench's vault) and SDB-101 (DataGrip's KeePass database) are cancelled**,
   with this ADR as the reason. What they found stays here and in the fixtures' READMEs. If
   this is reopened, they are written again from point 4: the crate first, then Workbench, then
   DataGrip with its reader.

## Alternatives
- **Yes, now, with the crate of point 4, for Workbench only** (the most reasonable
  alternative): about thirty lines of `unsafe` that a round-trip test covers, and no new
  dependency. It is small; it is also the smallest of the three audiences, and it spends the
  workspace's "no `unsafe` anywhere" and takes the risk of point 2's third reason for it.
- **Yes for all three.** DataGrip adds the KeePass reader; TablePlus's store would have to be
  reverse-engineered and would break with its next release.
- **Lifting `forbid` to `deny` for the whole workspace**, with an `allow` where needed. One
  line, and every crate could then carry `unsafe` behind an attribute; the lint would stop
  being a guarantee.
- **Asking the user to switch the other client to a store we can read.** DataGrip offers no
  other store on Windows.

## What would reopen this
- **A user asks for it**: the first request from someone who imported from DataGrip or
  Workbench on Windows. There is no telemetry to show it otherwise.
- **The Windows installers are signed** (ADR-063), which removes point 2's third reason.
- **A dependency already in the tree offers a safe DPAPI call**, which removes the second.

If it is reopened before the installers are signed, the first step is an experiment, not the
feature: an unsigned build that reads Workbench's vault, installed on a clean Windows with
Microsoft Defender on and submitted to a multi-engine scanner, compared with the same build
without the reader.
