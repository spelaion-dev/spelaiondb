# ADR-090: An SSH key's passphrase may be remembered in the system keychain, opt-in, under the key file's canonical path

- **Status:** Accepted
- **Date:** 2026-11-26
- **Amends:** ADR-009's keychain entry names (a third kind of name, `key:`)
- **Builds on:** ADR-007 (SSH in process), ADR-088 (an imported connection never names another
  entry)

## Context
An encrypted SSH key (OpenSSH, PEM or PuTTY's `.ppk`, SDB-528) asks for its passphrase every
time a tunnel opens, and again after the last connection using it closes or the app restarts.
Users of encrypted `.ppk` keys asked to stop typing it (SDB-534). A database password can
already be remembered in the system keychain from its dialog ("Remember password", ADR-009);
the key passphrase cannot.

ADR-009 fixed the names of the keychain entries: a repository's carry its canonical path
(`repo:<root>/<id>/password`, `repo:<root>/tunnel/<name>/<n>/password`) and a personal one starts
with `~/`, so a repository's files can never choose a name that holds another project's
password. A key passphrase belongs to neither: it belongs to a file on this computer, and
several connections, hops and folders may use the same key.

## Decision
1. **Opt-in, off by default.** The key passphrase dialog gets "Remember in the system keychain",
   unticked each time it opens, as the password dialog's "Remember password". It is offered
   only where there is a system keychain. Unticked, nothing is stored.

2. **One entry per key file, named after its canonical path:** `key:<canonical path>` (service
   `Spelaion`, as every entry of ours). Every connection, hop and folder that uses that file
   shares it, and moving or renaming the file means it is asked again. The name comes from the
   path the core resolved and canonicalized (`spelaion-config`'s `key_passphrase_account`),
   never from text a file writes.

   Sharing it across folders is safe because a key passphrase is never sent anywhere: it only
   decrypts the key file on this computer, and the key then signs for the hop. A repository
   that names the user's key (`identity_file`) can already make the app use it against the
   repository's server once the repository is trusted; remembering the passphrase changes
   when the person is asked, not what is used.

3. **Read before asking, written only once it worked.** When a key is encrypted, the core reads
   its entry first; if the passphrase decrypts the key, nobody is asked. A remembered one that
   does not (the key changed) is deleted from the keychain and the person is asked, with the
   same three attempts. A typed one is written only after it decrypted the key, through
   `secrets::store` (written and read back); a wrong one is never stored. An agent (Pageant,
   the OpenSSH agent) is still tried before the key file, as today.

4. **Only the desktop app reads it.** The SSH crate asks for it through its `Interaction`
   (`remembered_passphrase`, `remember_passphrase`, `forget_passphrase`, which read and write
   nothing by default); the desktop app's interaction reads and writes the keychain through
   `spelaion-config`. The MCP server's and the command line's interactions keep the defaults,
   so they never read a remembered passphrase (the MCP server answers no to every tunnel
   dialog already).

5. **It is a secret like any other.** It is held as `Secret`, never written to a log, a
   configuration file, the history, the audit log or an MCP response; the connection test's
   redaction learns it when it is read from the keychain as it does a typed one.

6. **Forgetting it.** The connection's SSH settings and the network profile's hop show "Forget
   remembered passphrase" for a key file that has one. Saving a hop whose key file changed
   forgets the old file's passphrase unless another hop of the loaded configuration still uses
   that file.

7. **Another entry for an import (ADR-088).** A `keychain:key:…` reference in a connections file
   names an entry that is not the item's own: the import refuses it and an export never
   carries it, as for `repo:` and `~/` names.

## Consequences
- A key used by many connections asks for its passphrase once, ever, when the person chooses
  so; the keychain's own protection (the session login, Credential Manager, Keychain Access,
  Secret Service) is what guards it, as for remembered passwords.
- A person who does not tick the box sees no change.
- Copying a key file to a new path, or a new computer, asks again: the name follows the path.

## Revisit if
- Users want one remembered passphrase per key regardless of where the file is (a fingerprint
  of the public key as the name instead of the path).
- The app gains an agent of its own that holds decrypted keys for the session.
