# ADR-055: A Plugins page fed by a signed catalogue of our plugins; other people's stay unsupported

- **Status:** Accepted (it changes how ADR-041's stages 3 and 4 are carried out; the core's
  part is implemented, 2026-11-14: see "Implementation"; the page, the download and the
  publishing workflow are not)
- **Date:** 2026-11-13
- **Supersedes:** in ADR-041, "No registry or marketplace" for our own plugins, and "from a
  file" as the only way to add someone else's (point 5); in ADR-042, "A plugin fix reaches
  users with the app release that moves its pin, never by itself" and, for what is downloaded,
  "The app carries the pins" (point 4)
- **Builds on:** ADR-041 (engines as plugins), ADR-042 (plugin repositories and pins),
  ADR-004 (no daemon), ADR-020 (each engine's barriers), ADR-021 (signed updates, asked for),
  ADR-022 (trust pinned to content)

## Context
ADR-041 and ADR-042 let the app download only engines of ours, the first time one is used,
accepted by a hash compiled into the app: a plugin fix waits for an app release, and there is
no place in the app that shows which engines exist. Other people's plugins are installed from a
file the user finds by themselves.

The owner wants (2026-11-13) a catalogue of our plugins inside the app, as Tabularis has
(Settings → Plugins: found, installed and updated with a click), and that anyone who wants a
plugin written by someone else can add it themselves, **without the project supporting it**. As
far as Tabularis's pages show (read on 2026-11-13), its list also carries other people's
plugins: authors connect their account to a registry service and a webhook brings in each
release. We did not find a step where a person reads a release before it is listed.

A plugin is a program that runs with the user's permissions and receives the passwords of its
connections (ADR-041). A list inside the app is the project saying "these are the ones to
install", so it can only hold what the project builds and tests. What must not change: security
lives in the core; nothing is installed without the user; nothing in a repository's `.spelaion/`
can bring a program in; an engine has read-only mode and agents only when its barrier is ours to
vouch for (ADR-020); and there is no server to run (ADR-004, ADR-021).

## Decision
**The app has a Plugins page. What it offers comes from one signed file, the catalogue, which
lists only our plugins and is written from this repository's pins. A plugin of someone else's
is never in it: the user adds it themselves, from a file or from an address they type, and the
app says everywhere that it is not ours.**

1. **The Plugins page** (Settings → Plugins) lists every engine of ours: inside the installer,
   installed, available, and with an update. Install, update and remove are one click each, and
   that click is the consent. The "add connection" picker shows the engines that are not
   installed and sends there. The page also lists the plugins the user added themselves, apart.
2. **The catalogue is a file, not a service, and it comes from here.** Every version it offers
   is one that this repository's `plugins.toml` pinned through a merged pull request, so it has
   passed ADR-042's gate (the core's every-engine tests through that build). For each version it
   says the protocol version, the lowest app version it works with, and for each platform the
   archive's address, size and SHA-256; it also lists withdrawn builds. When that changes on
   `dev`, a workflow here writes one file with a serial number that only grows, signs it and
   publishes it at a fixed address, as `latest.json` is. No accounts, no web form, no download
   counts, and no new repository.
3. **A key of its own signs it.** A minisign key like the update key (ADR-021), but another one:
   its public half is compiled into the app, and its private half is a secret of this repository
   used only by the workflow that publishes the catalogue. The update key signs nothing new. An
   app release can replace the catalogue key; the catalogue key can never replace the app.
4. **What the core checks, every time:** the catalogue's signature, and that its serial is not
   lower than the last one accepted; that it speaks the plugin's protocol version and is not
   older than the entry asks; that the archive is downloaded over HTTPS from a release of that
   plugin's repository under `spelaion-dev`; that its size and SHA-256 are the entry's. The hash
   is recorded on install and checked again before every start (ADR-042's check, with the pin
   coming from the catalogue). **The app reads no other catalogue:** a release build cannot be
   given another one, nor another address or key, by any setting, variable or repository file;
   `SPELAION_NO_UPDATES` can only switch it off. Each of these has a test that tries to get
   around it.
5. **Other people's plugins: added by the user, and not ours to support.** As ADR-041 left them
   (point 5): installed only by the user, after a dialog that says what it is (a program with
   your permissions that receives the passwords of its connections), pinned by its hash, with
   **no agents and no read-only mode**. The user gives the app a file, or **an address**:
   - The address is typed or pasted into the Plugins page, and nowhere else: no link, file
     association, command-line argument, repository file, connection file or agent can fill it
     in or start the download.
   - It is the HTTPS address of the plugin's archive, or of that plugin's own manifest naming
     the archive for each platform. It is read once, when the user asks: the app never goes back
     to it by itself.
   - The download is bounded in size and nothing in it is run or unpacked before the dialog,
     which shows the whole address, the host and the hash of what arrived, and says that nobody
     on the project has seen this program.
   - From there it is a file like any other: pinned by its hash, refused if it changes.

   To that this ADR adds what "without our support" means in the app:
   - It is labelled as not ours wherever an engine is shown: the Plugins page, the picker, its
     connections.
   - The app offers no updates for it. A newer version is another file or address the user
     gives, with the dialog again.
   - Its errors, and the connection test's diagnostics text, name the plugin and say it is not
     part of SpelaionDB, so a report goes to its author and not to us.
   - It is never in the catalogue. Becoming ours (its repository under `spelaion-dev`, built by
     our CI, its dialect and barriers in the core with their tests) is the only way in, decided
     one plugin at a time.
6. **A build of ours can be withdrawn** by a pull request here that says why. From the next
   catalogue the app accepts, it refuses to start that build, says so, and offers the one that
   replaces it.
7. **Updates are asked for, like the app's.** The app reads the catalogue when it checks for
   updates (the same personal setting, ADR-021) and when the Plugins page opens. An update is a
   notice; it is downloaded and installed only on a click, and the new build is the one started
   the next time that plugin starts: a running plugin is never swapped under its sessions.
   `spelaiondb mcp` never downloads: it starts what the app installed, with the same hash check
   and the last catalogue the app accepted.
8. **With no network, nothing is lost.** The engines inside the installer stay pinned in the app
   as ADR-042 says, and work with no catalogue. When the catalogue lists a newer build of one of
   them it is an update like any other, and the app starts the newest build it accepts. Which
   engines go inside the installer is decided with the sizes measured when DuckDB is migrated,
   not here. A plugin of ours can also be installed from a file: an archive whose hash is in an
   accepted catalogue (which can itself be given as a file, with its signature) is installed as
   that entry.
9. **Teams.** Nothing in a repository's `.spelaion/` can install a plugin or name a catalogue,
   an address or a key. A team connection whose engine is not installed says which plugin it
   needs: one of ours opens the Plugins page, where installing is the user's click; one that is
   not ours says so and stops (ADR-041).
10. **When.** The page, the catalogue and its key come with ADR-041's stage 3, in place of
    "download on first use". Adding other people's plugins, from a file or an address, stays in
    stage 4.

## Implementation (2026-11-14): the core's part
What points 4, 6, 7 and 8 ask of the core is written and tested
(`crates/spelaion-core/src/plugins/`: `catalogue.rs`, `installed.rs`, `choice.rs`; tests in
`crates/spelaion-test-plugin/tests/catalogue.rs`). The page, the download and the workflow that
writes, signs and publishes the catalogue are not. What was settled while writing it:

- **The catalogue has the shape of `plugins.toml`**: `schema_version`, `serial`, a `[[plugin]]`
  for every version ever pinned (the same keys, checked by the same code as a pin), and
  `[[withdrawn]]` entries (`name`, `version`, `reason`: one line, shown to the user). A
  withdrawn build need not stay listed. Its signature is a minisign signature file, over the
  file's hash (never the legacy form).
- **It carries no address** (point 2 said it would). The core writes an archive's address from
  the plugin's name, its repository and the archive's file name, all checked: always
  `https://github.com/spelaion-dev/<plugin>/releases/download/v<version>/<archive>`. A
  catalogue therefore cannot name another host or repository, even one we signed by mistake.
- **No key yet.** The public half is a constant of `catalogue.rs`, empty until an owner creates
  the key: a build without it accepts no catalogue, and the only plugins are the pinned ones.
- **The user's store is not trusted.** The accepted catalogue is kept with its signature and
  checked again at every read; a build in the store is started only if the pins or that
  catalogue name it, with the hash they give. Someone who can write in the personal folder and
  also choose what the app downloads could put back an older catalogue of ours (the serial is
  remembered only in that file); they still cannot have a program started that we did not build.
- **A plugin's archive is a `.tar.gz` on every platform** (the releases made `.zip` for
  Windows): one way to unpack, tested everywhere. Only the program is unpacked.
- **What is started** is the newest build on the machine that passes every check; of one
  version in both places, the one inside the installer. `min_app` is asked only of catalogue
  entries: a pinned build was tested with the very app that carries the pin.
- **The catalogue is written, never edited, and only grows.** A build is withdrawn with a
  `[[withdrawn]]` entry in `plugins.toml` (it cannot be the pinned build). The next catalogue
  is the published one plus what the pins add (`Catalogue::next`, run by
  `spelaion-write-catalogue`): a published version never changes, a withdrawn build stays
  withdrawn in every catalogue after it and cannot be pinned again, and the serial is spent
  only when something is new. The tool must be told which catalogue is published, and reads it
  only with our signature: a failed download cannot start the serial again.
- **A build that may not be started is said, not skipped**: when a plugin has builds on the
  machine and none passes, its engines answer with the reason (and the version that can be
  installed), and do not fall back to anything else.

## Consequences
- Our engines and their fixes reach users without an app release, and the installer can carry
  fewer of them. The app shows which engines exist.
- We vouch only for what we build and test. What other people write is the user's decision and
  its author's to maintain; the app says so at each step.
- **Other people's plugins are harder to find and to keep up to date than in Tabularis:** no
  list in the app and no update notice. That is the price of not standing behind them.
- Installing from an address saves the user a download with their browser, and with it the
  browser's and the system's own checks on downloaded programs: the dialog is the only warning
  left. "Paste this address into the Plugins page" becomes something a message can ask of a
  user, as "download and add this file" already was.
- A second key to keep. Whoever had it and could also write where the catalogue is published
  could offer a program to every user; it would still need their click.
- A plugin update has passed the gate against this repository's `dev`, not against each
  released app: for those, the protocol version and the entry's lowest app version are what
  cover it. More combinations of app and plugin versions exist than anyone tested together.
- Users see two classes of engine: ours, with read-only mode and agents, and the ones they
  added, without.
- The app makes one more request at startup, to the same host as the update check, under the
  same setting.

## Alternatives
- **Other people's plugins in the catalogue, after a review of each version:** more engines one
  click away, but we would be vouching for programs that receive database passwords, and
  reading every version of every one for good; "listed" would be read as "safe".
- **Letting the user add other catalogues** (an address that lists someone else's plugins and
  is read again and again): a list we do not write shown inside our app, with one-click
  installs and update notices for programs nobody here has read. An address for one plugin,
  read once, is what point 5 allows instead.
- **Other people's plugins only from a file** (ADR-041): the same decision by the user with one
  more step, the download in a browser. It keeps the browser's checks; the owner preferred the
  shorter way.
- **A registry service, as Tabularis has:** authors publish by themselves and versions appear
  without anyone reading them; a server and accounts to run and secure (ADR-004).
- **The catalogue in a repository of its own:** only worth it if other people's pull requests
  went there; with our plugins alone, the pins that feed it are already here.
- **Signing the catalogue with the update key:** one key, used far more often, and by a
  workflow that runs on every pin instead of only on a release.
- **Silent plugin updates:** a program that receives passwords replaced without the user
  knowing, and ADR-021 already says no for the app.

## Revisit if
- Users ask for engines we cannot write or test ourselves, and adding them one by one proves
  too awkward: listing other people's plugins after a review, or letting the user add another
  catalogue, gets its own ADR.
- Addresses are used to trick users into installing hostile programs: adding from an address
  is removed, and a file is again the only way.
- People ask for read-only mode or agents on plugins that are not ours: whether one can ever be
  vouched for gets its own ADR, as ADR-041 already says.
- Publishing the catalogue from `dev` lets through an update that breaks a released app: pins
  that feed the catalogue are then tested against the released versions too.
