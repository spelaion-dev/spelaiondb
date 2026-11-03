# ADR-022: Sharing connections with a team, safely and without passwords

- **Status:** Accepted (parts 1 to 3 implemented; parts 4 and 5 planned)
- **Date:** 2026-11-03
- **Amends:** ADR-009 (repository trust)

## Context
ADR-009 already lets a team version its connections in git without secrets: files in
`.spelunk/`, secrets only by reference (`keychain`, `prompt`, `env:`, `op://`, `cmd:`),
personal values in `${user.*}` variables, and nothing from a repository used until the user
trusts it. Teams of system administrators need more than that:

- **Trust is granted once and forever.** After trusting a repository, a `git pull` that
  changes a connection's `host` makes the app send the password from the user's keychain to
  the new server without asking. A colleague's mistake, or someone who gets write access to
  the repository, is enough.
- The first SSH connection of each administrator accepts the bastion's key unverified.
- Passwords are still the usual way in, even where the engines support certificates or
  short-lived tokens.
- A newcomer meets missing `${user.*}` variables as errors.
- Nothing checks the team repository before a change is merged.

## Decision
1. **Trust is pinned to content.** Trusting a repository records a snapshot of every
   connection and tunnel in it: all the fields of the file except its display `name`
   (comments and formatting do not count). Before a repository connection or tunnel is
   resolved, it is compared with its snapshot; if anything changed, or the item is new,
   nothing is resolved (no secret read, no command run) and the user is shown what changed,
   field by field, before and after. Accepting records the new snapshot for that item only.
   - Snapshots live with the trust in `trusted-workspaces.toml`, in the user's directory: a
     repository cannot write them. Repository files hold no secrets, so neither do they.
   - What the user changes through the app's forms is theirs: saving records its snapshot.
   - The app shows the change in a dialog; `spelunk mcp` refuses the connection and says
     to review it in the app or with `spelunk trust`; `spelunk trust` lists the changes it
     accepts.
   - Repositories trusted before this ADR have no snapshots: their current content is
     recorded the first time the app or `spelunk trust` loads them, as trust worked before.
2. **Shared server keys:** a reviewed `.spelunk/known_hosts` in the repository is checked
   before the user's own files, so a newcomer's first SSH connection is already verified. A
   different key stays a blocking error, even if the user's own file has the right one.
   - It is used only from a trusted repository and only as the user trusted it: the file is
     part of the snapshots of part 1 (one field per entry, comments not counted), so a pull
     that changes a key is reviewed before it is used. Testing a tunnel from its editor
     leaves an unreviewed file out (the hosts are asked about, as without one).
   - It is read, never written: keys the user accepts still go to their own file.
3. **Logins without passwords:** TLS client certificates (`tls.cert`, `tls.key`; the key
   from the user's directory, never the repository) and documented presets for short-lived
   tokens through `cmd:` (AWS RDS IAM, Azure AD, GCP). SSH certificates from a team CA later.
   - Client certificates for PostgreSQL, MySQL and MariaDB. SQL Server does not log SQL
     users in with one, so the file is refused for it. A repository connection whose key
     resolves inside the repository is refused before connecting; the form writes a key in
     the home directory as `~/…`.
   - Cloud tokens are a choice of the form (`PasswordChoice::CloudToken`) that writes a
     `cmd:` reference: `aws rds generate-db-auth-token` (with the region read from an RDS
     host name), `az account get-access-token --resource-type oss-rdbms` and
     `gcloud sql generate-login-token`. Only letters, digits, `.`, `-`, `_` and `@` from the
     host and user reach the command, so nothing can be injected into the shell. Reading the
     file back, a command that matches one of them shows as that token again. They are for
     PostgreSQL, MySQL and MariaDB; SQL Server takes Entra ID tokens differently (later).
4. **Onboarding:** missing `${user.*}` variables are asked for once, in a dialog, and saved
   to `user.toml`.
5. **`spelunk check`** for the team repository's CI and pre-commit: schema validation and
   refusal of risky patterns (a secret written inside a `cmd:`, a production connection that
   lets agents write).

## Alternatives
- **Trusting by path, as before:** simple, but it is the hole described above.
- **Hashes instead of snapshots:** they detect a change but cannot show it; the user would
  accept blind. Snapshots are small and hold no secrets.
- **Encrypted connection files (`sops`) or a sync server:** git stops being the reviewer and
  decryption keys must be handed out, the problem this design avoids; ADR-009 already rules
  out homemade encrypted files.
- **Several repositories open at once:** the team repository can be opened as the workspace
  for now. Revisit if administrators really work with several at the same time.

## Revisit if
The team uses a central secret manager (HashiCorp Vault, 1Password Teams): then part 3 and
native Vault support (pending in ADR-009) come before part 4.
