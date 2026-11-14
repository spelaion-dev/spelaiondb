# ADR-057: Installers carry our plugins as their release archives, installed into the user's store at the first start

- **Status:** Accepted
- **Date:** 2026-11-15
- **Supersedes:** in ADR-042, how "the release puts the pinned builds … inside the installers"
  (point 4): the installers carry the archives, not the programs. The CLI archives still carry
  the programs next to `spelaiondb`.
- **Builds on:** ADR-042 (pins), ADR-055 (the user's plugin store and its checks)

## Context
The first rehearsal of the release (2026-11-14, `release-test/v0.10.0-1`) built every installer
with the four pinned plugins next to the app, as ADR-042 said, and checked them against the
pins. Windows (NSIS, MSI), the `.deb` and every CLI archive passed. **The macOS apps and the
AppImage failed:** each of the four programs inside had another SHA-256 than its pin.

- **macOS:** Tauri signs the app (ad hoc, `signingIdentity: "-"`) inside out, and it signs each
  `externalBin` with `codesign --force`, which writes a new signature into the program.
- **AppImage:** linuxdeploy processes the executables it finds in the AppDir.

The core starts a plugin only while its file has the pinned hash. So on macOS and in the
AppImage no server engine would have opened. A pin cannot be taken after the bundler runs,
because a pin is one build that the plugin's repository released, CI tested and the catalogue
lists. Turning the check off for files inside the app would make the app the one place where a
changed plugin runs. And every bundler, signing step and notarisation to come could change the
programs again.

## Decision
**An installer carries each pinned build's release archive (`.tar.gz`), as the plugin's
release has it, in the installer's resources (`plugins/`). At every start, the app and the
`spelaiondb` that ships with it install into the user's plugin store (ADR-055) every pinned
build whose archive is there and that is not installed yet.** Nothing a bundler does changes an
archive: it is not a program, and nothing signs or patches it.

- **Only the pins compiled in are looked for**, each by its archive's file name in the pin. No
  other file of the folder is read.
- **The install is the store's** (`PluginStore::install_bundled`): the archive must have its
  pin's size and SHA-256 before anything in it is read; only the program is unpacked, and it
  reaches the store only with the pinned program SHA-256. A build the accepted catalogue
  withdrew is not installed. From there it is started as any installed build is: as the pins
  name it, its hash checked before every start.
- **Where the resources are** is computed from the program's own folder, as Tauri lays out each
  installer: `Contents/Resources/plugins` on macOS, `/usr/lib/SpelaionDB/plugins` (or the
  AppImage's) on Linux, `plugins` next to the program on Windows
  (`spelaion_core::plugins::bundled_folder`). The app and the CLI use the same rule. A test
  checks that the product name the rule uses is the app's.
- **The release** fetches the pinned archives with `spelaion-fetch-plugins --archives-into`
  (each one kept only after it and its program passed every check), and
  `tauri.release.conf.json` puts every `.tar.gz` there into the resources. A new pin needs no
  change to the release's files. The rehearsal checks that every installer carries each pinned
  archive, once, in a `plugins` folder, with its pinned SHA-256, and no plugin program.
- A program next to the app is still a plugin when the pins name it, so the CLI archives, the
  smoke tests and development builds work as before.

## Consequences
- The macOS apps and the AppImage open server engines again. Signing the app with a real
  identity, and notarising it, change nothing here.
- The first start after an install or an update unpacks four programs (about 100 MB) into the
  user's store, and the store keeps each version that was installed. Removing one from the
  Plugins page lasts until the next start, which installs it again from the installer while it
  is the pinned build.
- An installer is not usable by a user whose personal folder cannot be written. The store is
  already where installed plugins live, and that user could not run the app at all.
- `spelaiondb mcp` started before the app ever ran installs the plugins itself, through the same
  checks.

## Alternatives
- **Re-sign each plugin in its own repository exactly as Tauri will**, so that Tauri's signature
  leaves the bytes unchanged. This depends on `codesign`'s output never changing between runner
  images, and does nothing for the AppImage.
- **Build the macOS app with `--no-sign` and sign only the outer bundle.** The Intel plugins are
  unsigned, and a bundle with unsigned code in `Contents/MacOS` does not sign. It also does
  nothing for the AppImage.
- **Carry the programs as resources instead of `externalBin`.** Tauri does not sign resources,
  but linuxdeploy also processes the ELF files under the AppDir's `usr/lib`.
- **Hash a program without its code signature.** That means parsing Mach-O signatures in the
  check that every plugin start relies on, and it does nothing for linuxdeploy's changes.

## Revisit if
- A platform's bundler starts changing files that are not programs.
- Unpacking at the first start proves too slow or too large, for example once there are many
  plugins. Installers could then carry only some engines, and the Plugins page would install
  the rest.
