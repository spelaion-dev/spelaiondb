# ADR-021: Automatic updates, signed, asked for, off until there is a public source

- **Status:** Accepted
- **Date:** 2026-11-03

## Context
Each release means downloading and running the installer again. Tauri's updater plugin can
check a JSON file (`latest.json`), download the new installer, verify its signature and run
it. The repository is private, so its release assets cannot be downloaded without a
credential, and no credential may ship inside the app (a token in a binary is a published
token). The user has not chosen yet where public releases will live: this repository made
public, or a public repository that only holds releases.

## Decision
- **Tauri's updater plugin**, driven from the Rust side through our own commands
  (`update_check`, `update_install`): no updater permissions for the web view.
- **Signed updates:** every update artifact is signed with a minisign key (Tauri's
  `signer`). The public key is in `tauri.conf.json`; the private key is the repository secret
  `TAURI_SIGNING_PRIVATE_KEY` (no password: the key itself is the secret), and the user keeps
  a copy in a password manager. Losing it means installed copies can never update again;
  leaking it means anyone with the update source could ship code to every user. An update
  whose signature does not match is refused by the plugin.
- **Release artifacts:** only the release workflow builds them, with
  `src-tauri/tauri.release.conf.json` (`createUpdaterArtifacts`), so local builds need no
  key. `tauri-action` uploads the `.sig` files and `latest.json` to the release.
- **Never without the user:** the app may check at startup (a personal setting, on by
  default, in Settings → Updates) and shows a notice; downloading and installing happen only
  when the user clicks "Install and restart". After the restart nothing is re-run (AGENTS.md:
  open tabs come back with their SQL, not executed).
- **Off until there is a public source:** the list of endpoints in `src-tauri/src/update.rs`
  is empty, and with no endpoint the app says updates are not available in this build and
  never makes a request. Turning them on is one change: the URL of the public `latest.json`.
  Installs made before that change cannot update themselves; they need one manual install.
- **On since 0.4.0 (2026-11-04):** the repository is public (ADR-023), so the endpoint is
  `https://github.com/spelaion-dev/spelaiondb/releases/latest/download/latest.json`.
  GitHub's `/releases/latest` skips pre-releases, so releases are published as normal ones
  (marked "latest"); the version number (0.x) and the notes still say it is a preview.
  `SPELAION_NO_UPDATES` switches updates off for one run (tests, packaged builds updated by
  their own channel): it can only switch them off, never point them elsewhere. 0.4.0 is the
  first version that updates itself; earlier ones are installed by hand once.
- **What the updater can replace:** Windows (NSIS, run passively), macOS (`.app.tar.gz`) and
  Linux AppImage. `.deb`/`.rpm` installs are updated by their package manager, not by the app.

## Alternatives
- **A token in the app to read the private releases:** a published secret. Refused.
- **Checking a private source through a server of ours:** a server to run and secure, which
  ADR-004 (no daemon) and the project's scale argue against.
- **Silent updates:** convenient, but a database client must not restart in the middle of
  someone's work. The notice and the click stay.

## Revisit if
The user picks the public source (then fill in the endpoint and update this ADR), or the app
gets Authenticode/notarization signing (then the installers stop warning on first run).
