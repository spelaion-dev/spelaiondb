# Sharing connections with a team

Project connections and tunnels are plain files in `.spelaion/`: commit them, review them in
pull requests, and every colleague gets them with `git pull`. They never hold a password,
only where to find one (the keychain, an environment variable, 1Password, any command), and
personal values such as each person's database user go in `${user.…}` variables.

A repository's connections are not used until you trust it, and trusting is not forever
(ADR-022): SpelaionDB remembers each connection and tunnel as you trusted it. If a pull
changes one (another server, secret, tunnel or command) or adds a new one, it is not used
until you have seen what changed, field by field, and accepted it. Changes you make through
the app's forms are yours and need no review. `spelaiondb trust <folder>` does the same from a
terminal and lists the changes it accepts.

What differs from one administrator to another (their database user, their SSH user, the
name of their key) goes in `${user.…}` variables, such as `user = "${user.db_user}"`. The
first time a connection needs one you have not defined, SpelaionDB asks for every missing
value at once and saves it in the `[vars]` section of your `user.toml`, outside the
repository; `spelaiondb mcp` says which ones to add instead.

Commit the team's SSH server keys as `.spelaion/known_hosts` (the usual `known_hosts`
format; copy the lines from your own `~/.ssh/known_hosts` or SpelaionDB's after checking each
server once). They are checked before anyone's own files, so a newcomer's first connection
through a bastion is already verified instead of asking them to accept a fingerprint, and a
server that shows another key is refused. A change to that file is reviewed like a change to
a connection before it is used.

To log in without any password:

- **Client certificates** (PostgreSQL, MySQL, MariaDB): in the SSL tab, the certificate and
  its private key. The certificate may live in the repository; the key never does (SpelaionDB
  refuses a key inside the repository): keep it in your personal folder (`~/…`) or name it
  with a `${user.…}` variable, so each administrator uses their own.
- **Cloud tokens** (AWS RDS IAM, Azure Database with Microsoft Entra ID, Google Cloud SQL
  IAM): choose *Cloud token* as the authentication method and the provider. SpelaionDB writes
  the command that asks the provider's CLI (`aws`, `az`, `gcloud`, signed in on your
  computer) for a short-lived token, and runs it on every connection.

Check the team repository before a change is merged with `spelaiondb check` (it reads only the
repository, nothing personal, so it runs in CI). It fails on files that do not load,
references to tunnels that do not exist, malformed `${…}` variables, passwords written
inside a `cmd:` (`--password`, `PGPASSWORD=`, `user:pass@` in a URL…) and private keys in
the repository; it warns about production connections that let agents write or do not
verify the server's certificate, SSH keys inside the repository and broken
`.spelaion/known_hosts` lines (`--strict` fails on warnings too). In GitHub Actions, with the
`spelaiondb` binary from a release archive on the `PATH`:

```yaml
- run: spelaiondb check --strict
```

And as a git hook, in `.git/hooks/pre-commit`:

```sh
#!/bin/sh
exec spelaiondb check
```
