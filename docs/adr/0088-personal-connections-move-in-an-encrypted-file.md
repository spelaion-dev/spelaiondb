# ADR-088: Personal connections move between machines in a password-encrypted file: Argon2id and AES-256-GCM, secrets back to the keychain

- **Status:** Accepted
- **Date:** 2026-11-26
- **Builds on:** ADR-009 (secrets by reference, in the keychain), ADR-022 (a team shares
  through git and trust), ADR-030 (pasted connections are untrusted input), ADR-076 (an import
  never brings a program to run)

## Context
Moving to a new computer means adding every personal connection again and typing every password
again (SDB-31). A repository's connections already move: they live in its files, and a team
shares them through git with trust pinned to their content (ADR-022). Personal connections live
in the personal folder (`connections/`, `tunnels/`) and their passwords in the system keychain,
which no other machine can read.

The file that carries them holds passwords, so it has to be encrypted with something the user
chooses and remembers, it has to refuse a changed file instead of reading it, and the passwords
must end up in the new machine's keychain, not in a plain file. ADR-009 rules out "homemade
encrypted files" as a place to keep secrets instead of the keychain; this file is not one: it is
made on request, moved by the user, and read only by an import that puts each secret into the
keychain.

## Decision
1. **Only personal connections, and the personal network profiles they go through.** The user
   chooses which connections (all of them by default). A repository's connections and network
   profiles are never in the file: they move with their repository, and trust stays pinned to
   their content there (ADR-022). Nothing else is in it: not the trust store, not `known_hosts`
   (the new machine asks for each server's fingerprint on the first connection, as it does for
   any new machine, and a changed one still blocks), not history, audit or other local data. The
   other personal settings move with the settings export, which holds no secrets (SDB-31).

2. **What the file carries:**
   - each chosen connection's file and each personal network profile it names, as the text on
     disk (TOML) with its id or name;
   - the value of every keychain entry those files name: the connection's password
     (`password = { secret = "keychain" }`, its default entry `~/<id>/password`, or a named
     `keychain:<name>`), each hop's password and the proxy's password of the profile.
     A reference to `env:`, `op://`, `cmd:` or `prompt` is carried as written in the file, and
     no value is read for it. An entry that does not exist is left out, so its password is asked
     on the new machine. Without a system keychain the export carries no values and says so.

3. **The format** (`.spelaion-connections`), a binary file:

   | Bytes | Field |
   |---|---|
   | 8 | magic, ASCII `SPDBCONN` |
   | 1 | format version, `1` |
   | 4 | Argon2id memory in KiB, unsigned little-endian |
   | 4 | Argon2id iterations, unsigned little-endian |
   | 4 | Argon2id lanes, unsigned little-endian |
   | 16 | salt, random |
   | 12 | nonce, random |
   | rest | AES-256-GCM ciphertext followed by its 16-byte tag |

   The header (every byte before the ciphertext) is the cipher's associated data: changing any
   byte of it, as much as the ciphertext, fails authentication. The plaintext is one UTF-8 JSON
   document: `{"version": 1, "app_version": …, "exported_at": …, "connections": [{"id", "file",
   "secrets"}], "network_profiles": [{"name", "file", "secrets"}]}`, where `secrets` maps the
   keychain entry's role (`password`, `hop:<n>`, `proxy`) to its value. A file larger than
   32 MiB is refused before any work.

4. **Key derivation: Argon2id** (version 0x13, RFC 9106) with 64 MiB of memory, 3 iterations and
   4 lanes (RFC 9106's second recommended setting), a 16-byte salt from the operating system's
   random source, new for every export, and a 32-byte key. The parameters are in the header so a
   later version can raise them; an import accepts memory from 19 MiB to 1 GiB, 1 to 10
   iterations and 1 to 16 lanes, and refuses anything outside as a file it cannot read, so a
   file cannot make the import use unbounded memory or time. The export asks for the password
   twice and the core refuses one shorter than 12 characters; the import takes any length.

5. **Authenticated cipher: AES-256-GCM** with a 12-byte random nonce. The key is new for every
   file (a new salt), and a file is encrypted once, so a nonce is never used twice with a key
   even if the random source repeated one.

6. **The crates:** `argon2` 0.6 and `aes-gcm` 0.11, from RustCrypto, both `MIT OR Apache-2.0`
   and already allowed by `deny.toml`. Both are already in `Cargo.lock` through `ssh-key` and
   `ssh-cipher` (russh, ADR-007), so naming them adds no crate to the build, and the versions are
   kept equal to the ones russh brings so they stay one copy each. The salt and nonce come from
   `getrandom` 0.4, already there through `uuid`; the key, the password and the plaintext are
   held in `zeroize`'s `Zeroizing` buffers and dropped as soon as they are used. Considered:
   `age` (a well-known format, but its passphrase mode is scrypt with its own work factor and it
   brings a crate tree we do not have), XChaCha20-Poly1305 (`chacha20poly1305` would be a new
   crate, and a nonce collision is no risk with one message per key), and `ring` (no Argon2).
   `cargo deny check` runs in the pull request that adds them.

7. **A wrong password and a changed file fail the same way, cleanly.** GCM cannot tell them
   apart, so both read "The password is wrong, or the file was changed after it was exported".
   A bad magic, an unknown version, parameters out of bounds, a truncated file or a plaintext
   that is not the expected document read "This is not a SpelaionDB connections file, or a newer
   version made it". In every case nothing is written: no file, no keychain entry. No error,
   log or report holds the password, a secret or the decrypted text; secrets are held as
   `Secret` from the moment they are decrypted.

8. **The import checks everything before it writes anything.** Each connection and profile file
   goes through the same loading and checks as a file on disk (unknown keys refused, values
   bounded), and its id or name through the same rules as one typed in the form. Then the
   preview lists each item: new, or replacing one with the same id (the user chooses to skip it,
   the default, or to replace it). As with a pasted connection (ADR-030) and a network profile's
   program (ADR-076), **an import never brings anything that runs a program**: a connection whose
   password is `cmd:…`, that names a `sqlite3` or `kubectl` program, or whose profile has a hop
   `command`, a `cmd:` password or names an `aws` or `kubectl` program, is listed as not
   imported with the reason, and the user adds it again on this machine. Nor does it bring
   anything that could send a secret from somewhere else to a server the file chooses: a
   password read from `env:` or `op://`, a `keychain:<name>` that is another connection's or
   profile's entry (`repo:…`, `~/…`), or a `${…}` variable other than `${user.…}` (which could
   put the environment into a host name, as ADR-030 refuses for a paste). An export never carries
   another connection's entry either.

9. **Secrets go to the keychain, never to a file.** Each item is written whole or not at all: its
   secrets first, each through `secrets::store` (written and read back), then its file through
   `atomic_file`. When a step fails, the keychain entries it changed get their previous values
   back (or are deleted when there were none), its file is not written, and the report names the
   item and why; the items already written stay. A secret for a `cmd:`, `env:`, `op://` or
   `prompt` reference is never in the file to begin with. The configuration is read again after
   the import, as after any change from the form.

10. **Only the user, only in the app.** Export and import are actions of the desktop app, with
    the file chosen in the system's dialog. The MCP server has no tool for either, and agents
    never see the file, its password or its contents. The app keeps no copy of the file and
    never remembers its password.

11. **The code lives in `spelaion-config`**, beside the secret backend and the loaders:
    `connections_file.rs` seals and opens the format (no file system, no keychain) and
    `connection_transfer.rs` gathers the export and writes the import. Both are added to
    `.github/CODEOWNERS` in the pull request that adds them, and any change there needs tests
    that try to get around it: a round trip, a wrong password, every header field and the
    ciphertext changed, a truncated file, parameters out of bounds, an item that runs a program,
    and a keychain failure halfway through an item.

## Consequences
- A new machine gets its personal connections, profiles and passwords from one file and one
  password. The first connection to each SSH server asks for its fingerprint again.
- Anyone with the file and its password has those passwords: the export dialog says so, and
  that the file should be deleted once imported. Argon2id's cost makes guessing slow, not
  impossible, so the 12-character minimum matters.
- Team sharing is unchanged: git and trust (ADR-022). The file is not a way to share connections
  with others and the app does not present it as one.
- Connections that run a program must be added again by hand. That is deliberate.

## Revisit if
- Users ask to keep machines in sync rather than move once: that is a different feature
  (ADR-009 still says git for shared files).
- The Argon2id setting is too slow on the machines users have: the header lets a later version
  change it, and an older file keeps reading.
- A standard format with a passphrase mode we trust (age) comes into the build for another
  reason.
