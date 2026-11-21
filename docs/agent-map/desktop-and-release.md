# Desktop and release

- `apps/desktop`: Tauri 2 + React app. `src-tauri` is a thin adapter over the core; the
  command types are generated into `src/bindings.ts`
  (`cargo run -p spelaion-desktop --features bindings --bin export-bindings`; CI checks it is up to date).
  `fetch_window` is the only command outside tauri-specta (binary Arrow, cells already as
  text); its error is the generated `WindowErrorItem` (freed or failed, SDB-367), which the
  grid's row cache keeps per block instead of asking again at every paint, and a freed result
  shows "Run again" where the grid is. A result's end reaches the page as the
  `resultEnded` script event (`ResultEndItem`); `src/resultEnd.ts` words it, and the page never
  works it out from a statement's failure. Every other command is `#[command]` (`crates/spelaion-desktop-macros`), never
  `#[tauri::command]` by hand: it adds `#[specta::specta]` and runs an async command's body
  through `src-tauri/src/panics.rs`'s `guard`, so a command that panics answers the page with an
  error instead of leaving it waiting (SDB-163); an async command therefore returns `CmdResult`.
  Sync commands are caught by the invoke handler (`panics::catch_sync`). Our own canvas grid (`ResultGrid.tsx`): past the browsers' 33,554,428 px height cap
  its spacer stops at `MAX_SPACER_H` and the scroll is scaled to the rows (`src/grid/scroll.ts`),
  so a row position is always read with `contentTop` and set with `scrollToContent`, never with
  the container's `scrollTop`. App commands (name, shortcut, menus) are
  registered once in `src/commandRegistry.ts` and run through `src/commands.ts`: the tab and
  file menus, the activity bar, the global shortcuts and the command palette
  (`CommandPalette.tsx`, Ctrl+Shift+P) all read it, and `scripts/commands.test.mjs` checks the
  menus against it. The palette's objects are only what the explorer already read (`catalog.ts`).
  In the SQL editor, completion, Ctrl+click and hover resolve a name with one reader of the
  statement (`src/sqlCompletion.ts`: `completeSql` and `resolveName`, tested by
  `scripts/sql-completion.test.mjs` and `scripts/sql-names.test.mjs`) over the catalog the tab
  read in the background (`useCompletionCatalog.ts`): never a query per key or hover, and a
  name that is not certainly one table or column shows nothing. They are hints: nothing is run
  or decided from them. Smoke test against the real app:
  `node apps/desktop/e2e/smoke.mjs` (after building the app and the CLI in one command, so
  nothing is built twice: `npx tauri build --no-bundle -- -p spelaion-desktop -p
  spelaion-cli -p spelaion-plugin-fetch`; a release build on 16 GB needs `CARGO_BUILD_JOBS=3`,
  see `CONTRIBUTING.md`).
  It drives WebView2 over CDP, so the whole test runs on **Windows** only. Its window is moved
  left of every monitor as it opens and never takes the foreground (`SPELAION_E2E_OFFSCREEN`, read only by
  `open_main_window`), so the user's real mouse and keyboard cannot reach it; never make a step
  depend on the window being visible or focused (`SPELAION_E2E_SHOW_WINDOW=1` shows it). On **Linux**,
  `node apps/desktop/e2e/smoke-linux.mjs` drives the same release build through
  `tauri-driver` and WebKitWebDriver (`e2e/webdriver.mjs`, no dependencies): a shorter pass
  (start, trust, a query on every engine, the real clipboard both ways, an SSH tunnel, quitting
  with a transaction open), with what it needs in `CONTRIBUTING.md`. CI runs this Linux pass
  (`.github/workflows/smoke-linux.yml`, not a required check) on every pull request and push
  to `dev` that touches more than documentation: a release build on
  ubuntu with `testing/docker` up, under Xvfb with Openbox, so it takes the X11 paths (`xclip`
  for the clipboard, `wmctrl` for the close request). A failure names the step and uploads the
  screenshots and the log (artifact `smoke-linux`). macOS has neither (no
  WebDriver for WKWebView). `e2e/processes.mjs` closes windows and lists processes for both.
  Automatic updates (ADR-021): `src-tauri/src/update.rs` (Tauri's updater plugin, reading
  `latest.json` from the latest GitHub release, so releases are never pre-releases;
  `SPELAION_NO_UPDATES` switches them off, as the smoke test does). Update artifacts are signed only in the release workflow
  (`tauri.release.conf.json`, secret `TAURI_SIGNING_PRIVATE_KEY`); never commit a private key.
  Installers: `npx tauri build` (NSIS + MSI on Windows). Releases: pushing a tag `vX.Y.Z`
  runs `.github/workflows/release.yml` (installers for every OS, the `spelaiondb` CLI, notes from
  `docs/releases/vX.Y.Z.md` plus every commit since the previous tag, from
  `scripts/release-notes.sh`, so commit subjects must read well in a changelog); the tag must
  match the version in `Cargo.toml` and `tauri.conf.json`. Pushing a branch `release-test/<x>`
  runs the same workflow as a rehearsal: the same installers and CLI archives, uploaded as the
  run's artifacts and checked by `scripts/check-release-plugins.sh` (each pinned plugin next to
  `spelaiondb` with its pinned hash). Its jobs only read the repository, never name a secret and
  build without update artifacts (`tauri.rehearsal.conf.json`); only the tagged jobs have
  `contents: write`. Pushing such a branch is a maintainer's step, never an agent's.
- Lifecycles in the app (ADR-060): `src-tauri` hands on the core's state types, and the page
  reads them from `bindings.ts`; it does not rebuild a state from events or derive it from
  flags. A state that exists only in the page (a dialog's phase, a value being loaded) is a
  discriminated union whose `switch` ends in a `never` check. The shared ones are in
  `src/dialogState.ts` (`Loaded<T>`, "preview, then apply", a job's phase; reducers tested by
  `scripts/dialog-state.test.mjs`) and their hooks in `src/useDialogState.ts`
  (`usePreviewThenApply`, `useJobPhase`, `confirmShown`): a new dialog uses them, not its own
  preview, error, busy or id flags. The hooks only show: what may run is still the core's
  check (ADR-047, ADR-008). The search panel's data search is one phase in `src/searchRun.ts`.
  The app's own lifecycles (the registry of running work, a tab's connection, the dialogs, the
  update, the folder switch, quitting) are SDB-369, SDB-371, SDB-375 and SDB-377.
  SDB-371: a tab's connection is one union, `Tab.connectionState` (`src/tabState.ts`'s
  `TabConnection`: disconnected with why the last attempt failed, connecting, connected with its
  version, transaction and "read-only for this session", lost, idle-closed), so a transaction or
  a read-only mark cannot outlive its session. It changes only through the store's
  `setConnection`, `setTransaction` and `setSessionReadOnly` with `tabState.ts`'s transitions
  (`connectedWith`, `connectFailed`, `sessionLost`…), and `openSession` returns the version and
  the transaction together. Components read a tab's connection, its run (`isRunning`,
  `cancelling`, `runStateOf`) and its edits only through `tabState.ts`'s selectors ("busy",
  "can start a run" — what the Run button and the palette both use —, "lost", "connection in
  use", "open for the explorer", "a question is open"); `scripts/tab-state.test.mjs` fails the
  build when a file other than `tabState.ts` or `store.ts` reads the raw fields or writes a
  connection state by hand.
  An editor tab's run state reaches the page as the `runChanged` event (`RunStateItem`,
  forwarded by `forward_run` only while the session is the tab's, SDB-370) and is kept as the
  tab's `runState`: the status line says "Paused at statement 3" and "Cancelling…" from it
  (`src/runState.ts`), and Cancel is offered again only once the core no longer says it is
  cancelling. A statement's ending is `StatementEndItem`; `ScriptFinished` carries only its
  summary.
  SDB-377: the update's state is `src-tauri/src/update_state.rs` (unknown, switched off, up to
  date, available with its install ready, downloading or failed, or not installed by the app),
  so a failed download is installed again without a new check and its progress reaches the page
  (`UpdateChanged`); `update.rs` keeps the check, the signature and `SPELAION_NO_UPDATES`.
  `app_lifecycle.rs` holds quitting (`Quit`: idle, sent, asking, quitting; the window's event
  loop only touches atomics, never waits) and the folder switch (`Folder`, whose `FolderSwitch`
  guard settles it when dropped, so a failed or panicking switch never leaves the app unable to
  change folder). After a plugin change only new connections use the new engines; a session
  already open keeps its plugin process until it is closed (ADR-055), on purpose.
  Running work (exports, data searches, column statistics, imports, backups, restores, file
  runs, connection tests, row loads, plugin installs) is one registry, `AppState.jobs`
  (`src-tauri/src/jobs.rs` over the core's `JobRegistry`, SDB-369): a job command takes a
  `job: Channel<JobTicket>` and sends the ticket (an id the registry made, and the kind) when
  the job starts; the page never makes an id (`src/jobs.ts`' `startJob` holds a Cancel pressed
  before the ticket arrives; `useJobPhase` and `searchRun.ts` keep the `Job`) and stops any job
  with `cancelJob(id, kind)`. A new job is one `jobs::start` call, held until it ends; no map
  to add or remove from.
- The frontend build checks token use in `styles.css` and the canvas code with
  `scripts/check-ui-tokens.mjs`. Its counted baseline records existing declarations; remove
  each fixed occurrence from `scripts/ui-tokens-baseline.json` in the same change.
  `node scripts/check-ui-tokens.mjs --print-baseline` prints the current counts for comparison.
  Pull request CI compares the counts with the base commit so an entry can only decrease.
  `scripts/contrast.test.mjs` checks text, icon and focus token pairs in both themes.
