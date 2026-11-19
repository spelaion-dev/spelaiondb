# Plugins

- The plugin protocol (ADR-041, `docs/plugin-protocol.md`: the specification, which the code
  must match; experimental until 1.0). An engine plugin is a program the core starts and
  talks to over its standard input and output: JSON-RPC 2.0 messages for control, Arrow IPC
  for rows (never JSON), in frames. Three crates:
  - `crates/spelaion-plugin-protocol`: the wire only. `frame.rs` (frames, at most 64 MiB,
    refused before anything is allocated), `message.rs` (every method's parameters and the
    driver's error) and `ipc.rs` (`BatchDecoder` reads one IPC message at a time because Arrow's
    own `StreamDecoder` does not see a schema until a batch follows it). A change that breaks
    is a new major `VERSION`; within one, changes only add.
  - `crates/spelaion-plugin-sdk`: the plugin's side. A Rust plugin implements `Driver` and
    `Connection` and calls `run`. It sends a batch only with credit from the core, reports a
    connection known to be closed, answers even when a driver panics and ends the program when
    its input closes: first its requests stop, its sessions are dropped and each driver's
    `Driver::shutdown` closes what it keeps open (DuckDB's shared files write what they hold),
    so a file is never left half written by a plugin that was only asked to end (SDB-360).
    Standard output is the protocol: a plugin never prints to it.
  - `crates/spelaion-plugin-host`: the core's side. `Plugin` is one process per program,
    started on the first connection and shared by every session of its engines. Its pipes are
    read and written on threads of their own with blocking calls, as the SDK's are (never
    `tokio::spawn` or tokio's own pipes there: the runtime that connected first may end before
    the others, and each read would be a trip through the runtime's blocking pool); `PluginDriver`
    and its connection are the `Driver` and `Connection` the core already uses. Its process has
    one state, `Plugin::state()` (`ProcessState`: not started, starting, running, stopping, dead
    with why), changed only by `Plugin::process` and `Plugin::stop`; a process that dies on its
    own reads as dead at once (`tests/process_state.rs`). A plugin that
    stops loses its sessions (`is_closed`, ADR-028) and nothing is re-run. Stopping one
    (`Plugin::stop`, or the last of it dropped) closes its input and kills it only if it has
    not ended after `STOP_GRACE` (5 s): a plugin killed at once could leave a file half written. The password goes
    only in `connect`, and every error, notice and log line of a plugin has the secrets it was
    given taken out. What a running plugin's process uses (`Plugin::usage`, SDB-399: memory,
    processor time, sessions) is read for that one process with safe calls only (`usage.rs`: a
    job object through `win32job` on Windows, `/proc` on Linux, `ps` elsewhere), never by
    starting it; the core lists them (`plugins::running`) and Settings › Plugins' Restart is
    `plugins::stop`. `conformance.rs` is the set a plugin runs against itself in its own CI
    (ADR-042).
  `crates/spelaion-test-plugin` is a made-up engine served as a plugin, never shipped; its
  `tests/protocol.rs` runs the protocol end to end (results, credit, cancel, a plugin that
  dies, another version, the password in errors, logs, arguments and environment): any change
  to the three crates needs its test there. In the core, `plugins.rs` decides which program is
  started: one `Plugin` per file for the whole program (`plugin_at`), and
  `SPELAION_DEV_PLUGIN_<ENGINE>` points a development build at a plugin built on this machine;
  a release build ignores it (`dev_plugin`), and nothing in a setting or a repository file can
  name a plugin.
  `plugins/pins.rs` holds the pins (ADR-042): `plugins.toml` at the root, compiled into the
  core, names the builds of our plugins this repository tests and ships. Only a pinned program
  next to the app is ever a plugin (`Engines::with_pinned_plugins`), and its SHA-256 is checked
  every time its process is about to start (`check_file`, through the host's `with_check`): the
  one check, which the catalogue's plugins and a user's own will use with their own hash
  (ADR-055). Like `trust.rs`, any change there needs tests that try to get around it
  (`crates/spelaion-test-plugin/tests/pins.rs`).
  The catalogue of ADR-055 is three more files there, with no page, download or publishing
  workflow yet. `plugins/catalogue.rs`: a file shaped like `plugins.toml` (every version ever
  pinned, a serial, the withdrawn builds), read only after its minisign signature by the key
  compiled in (`SHIPPED_KEY`, whose secret half is only in the `plugins-catalogue` environment,
  usable only from `dev`; never the update key), and
  it carries no address: an archive's is written from the checked name and repository
  (`PinnedPlugin::archive_address`). `plugins/installed.rs`: the user's store
  (`<personal folder>/plugins`), which is not trusted: the kept catalogue is checked again at
  every read and never replaced by one with a lower serial, and an archive (`.tar.gz` on every
  platform) is installed only as the entry whose hashes it has, only its program unpacked.
  `plugins/choice.rs`: for each plugin, the newest build on this machine (next to the app or
  installed) that the pins or the accepted catalogue name, that speaks the core's protocol, that
  this app is not too old for and that was not withdrawn; when builds are here and none may be
  started, its engines say why and do not fall back; when the pinned build's program is not
  next to the app, an engine served only by its plugin says which file is missing (one whose
  driver is compiled in is still served by it). `PluginState::availability` is the one answer
  of what its engines get (in use, refused, missing, not installed: a build in use wins, then
  the newest refused, then the missing pin), and `message` is written from it. The app and
  `spelaiondb mcp` take their
  engines from `Engines::builtin_with(store)`; neither downloads. The installers carry the
  pinned builds as their release archives, never the programs (ADR-057: a signed macOS app and
  an AppImage change the programs inside): `builtin_with` first installs into the store each
  pinned archive found in the installer's resources (`plugins::bundled_folder`,
  `PluginStore::install_bundled`), with the store's checks. Any change to the three needs
  tests that try to get around it (`crates/spelaion-test-plugin/tests/catalogue.rs`, which signs
  its own catalogues with a key of the test).
  The catalogue is written, never edited: `Catalogue::next` gives the text that follows the
  published one from the pins, and **only adds** (a published version never changes, a
  withdrawn build stays withdrawn and is never pinned again, the serial is the next one).
  `plugins.toml` withdraws a build with a `[[withdrawn]]` entry and its reason; it cannot be the
  pinned build. `spelaion-write-catalogue` (in `crates/spelaion-plugin-fetch`, never shipped) is
  that function as a program for the publishing workflow: it must be told which catalogue is
  published (its one signed file, checked with the key compiled in) or `--first`, writes
  nothing when nothing is new, and signs nothing. `--check` is the last step before publishing:
  the signed file must be accepted with the key compiled in and hold the text written.
  `.github/workflows/plugins-catalogue.yml` runs it on `dev` when `plugins.toml` changes (or by
  hand), signs with minisign in the `plugins-catalogue` environment and publishes the release
  `plugins-catalogue`'s one asset (never the latest release, which the updater reads).
  `plugins/fetch.rs` brings the catalogue and an archive, and the core still has no HTTP
  client: the adapter gives a `Transport` that only moves bytes. No function there takes an
  address: the catalogue's is a constant and an archive's is the one the core writes for its
  entry. HTTPS where it starts and where it ends, a catalogue bounded and an archive of exactly
  its entry's size, a stalled download given up on, the file removed whatever happens, and what
  arrives goes through the same checks as a file the user picked. The catalogue is read only
  when `updates_allowed` (the user's setting; `SPELAION_NO_UPDATES` can only switch it off); an
  archive only on the user's click. Tests with a made-up network in the same `catalogue.rs`.
  `crates/spelaion-plugin-fetch` (`spelaion-fetch-plugins`, never shipped) is how the pinned
  builds get here, for CI, the release and a developer: it downloads each pinned release from
  its plugin's repository and lets a program into the folder it is given only if the archive
  has the pinned size and SHA-256, holds the program at its top and that program has the
  pinned SHA-256; then it prints the `SPELAION_DEV_PLUGIN_<ENGINE>` lines that send the tests
  through them. No secret, and no new dependency (`curl` and `tar`). Any change there needs
  tests that try to get around it (`tests/fetch.rs`).
  Until an engine's driver moves to its own repository, its crate here also builds its plugin
  program (`src/bin/spelaion-plugin-<engine>.rs`; MySQL's serves MariaDB too), and
  `scripts/measure-plugins.sh` measures ADR-041's gate (a million-row result and a million-row
  import, in process and through each plugin: at most 15 % longer), and
  `scripts/test-through-plugins.sh` runs any `cargo test` with every engine through those
  programs: a change to the protocol's three crates or to a driver is tested both ways. A
  database on another machine (SQLite over SSH, ADR-018) never goes through a plugin: the core
  keeps the driver compiled in for it (`ThroughPlugin` in `open.rs`).
