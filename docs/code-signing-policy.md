# Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io), certificate by
[SignPath Foundation](https://signpath.org).

> **Status:** SpelaionDB is applying to SignPath Foundation's open-source programme
> ([ADR-063](adr/0063-signing-windows-installers-and-notarizing-macos.md)). Until it is
> accepted and the release workflow signs through it, the Windows installers are not signed.
> This page is the policy those signatures will follow, published before the first one as
> SignPath asks.

## What is signed

On Windows, each release signs:

- the desktop app's executable and the `spelaiondb.exe` it carries (the command-line tool and
  MCP server);
- the installers built around them: `SpelaionDB_<version>_x64-setup.exe` (NSIS) and the
  `.msi`;
- the `spelaiondb.exe` in the `spelaiondb-<version>-x86_64-pc-windows-msvc` archive.

Only programs built from this repository's source are signed. Not signed under this policy:
the engine plugins' programs, which are built and released from their own repositories under
[spelaion-dev](https://github.com/spelaion-dev) and carried inside the installers as archives
([ADR-057](adr/0057-installers-carry-plugin-archives.md)); the app checks each one's SHA-256 before
it starts it.

The macOS app is signed and notarized with an Apple Developer ID, not by SignPath. Every
release's update artifacts also carry the updater's own signature
([ADR-021](adr/0021-automatic-updates.md)), which the app checks before installing an update.

## How it is built

- Every signed file is built by [`release.yml`](../.github/workflows/release.yml) on
  GitHub-hosted runners, from a `v*` tag on `main`. Nothing built on a person's machine is
  signed.
- The build scripts and the CI configuration live in this repository with the source. Changes
  to `.github/workflows/` need an owner's approval
  ([`CODEOWNERS`](../.github/CODEOWNERS)).
- Each release's signing request is approved by hand, by an approver, in SignPath. A request
  that does not come from that workflow and a tag on `main` is not approved.

## Team roles

- **Committers and reviewers:** the [members of the spelaion-dev organization](https://github.com/orgs/spelaion-dev/people)
  and the repository's collaborators with write access. `dev` and `main` change only through
  pull requests whose CI passes; a pull request from someone outside the team is reviewed by a
  committer before it merges. How changes are made and reviewed is in
  [`AGENTS.md`](../AGENTS.md) and [`CONTRIBUTING.md`](../CONTRIBUTING.md).
- **Approvers:** the [owners of the spelaion-dev organization](https://github.com/orgs/spelaion-dev/people?query=role%3Aowner).

Every member uses multi-factor authentication on GitHub and on SignPath.

## Privacy policy

SpelaionDB collects nothing about you: no telemetry, no analytics, no crash reports, no
account. It contacts other systems only in these cases:

- **Your own servers.** It connects to the databases, SSH bastions and proxies you configure,
  when you connect to them. `spelaiondb mcp` runs on your computer and reaches only the
  connections you exposed to AI agents.
- **The update check.** With "Check for updates when the app starts" on (Settings → Updates,
  on by default), the app reads, from GitHub, the latest release's
  [`latest.json`](https://github.com/spelaion-dev/spelaiondb/releases/latest/download/latest.json)
  and our [plugin catalogue](https://github.com/spelaion-dev/spelaiondb/releases/download/plugins-catalogue/plugins-catalogue.signed)
  ([ADR-021](adr/0021-automatic-updates.md), [ADR-055](adr/0055-plugins-page-and-signed-index.md)).
  It reads the catalogue again when you open the Plugins page or check by hand. Each request
  carries no cookie, no identifier and nothing of yours, only the user agent
  `SpelaionDB/<version>`; GitHub sees your IP address, as with any download
  ([GitHub's privacy statement](https://docs.github.com/site-policy/privacy-policies/github-general-privacy-statement)).
  Turn the setting off and the app makes no request until you check by hand. The environment
  variable `SPELAION_NO_UPDATES` switches the checks off entirely.
- **Downloads you ask for.** An update, one of our engine plugins or a plugin from an address
  you type is downloaded only when you click to install it.
- **Map tiles.** Off by default. If you turn them on and name a tile server (Settings → Map),
  the map view loads its tiles from that server, which then learns which part of the world you
  are looking at.
- **Links.** "Report a problem" and the links in the app open your browser; nothing is sent
  with them.

Nothing else leaves your computer.
