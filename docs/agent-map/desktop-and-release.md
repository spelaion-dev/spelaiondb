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
  menus against it. The user can change a command's keys (SDB-447, ADR-064): read a command's
  keys with `commandKeys` (the user's or the default), never its `shortcut` alone, and bind the
  SQL editor's keys through `editorKeyBindings` (`editorActions.ts`). The user's changes come
  from the core (`shortcutPreferences.ts`), which checks the keys; the registry judges clashes
  by place (`shortcutClash`). The palette's objects are only what the explorer already read
  (`catalog.ts`).
  A table's or a view's actions live in `src/explorerActions.ts` (SDB-92): its explorer menu
  (`ObjectNode` in `Sidebar.tsx`, each item marked with `data-action`) and the palette's
  `relationActions` call the same functions, Rename…, Drop…, Duplicate table… and generated SQL
  open the same dialogs (`ExplorerDialogHost`), and the test compares the two lists. A new item
  in that menu goes in both, or in the test's list of menu-only items with its reason. The same
  holds for a schema's menu (`SchemaNode`, `schemaActions`) and a routine's, sequence's or
  trigger's (`SchemaObjectNode`, `schemaObjectActions`, SDB-441); a column's and an index's menus
  (`ColumnRow`, `IndexNode`) are compared too, all menu-only, since the palette does not list
  them. Every item of those menus carries its action (`data-action`), or the test fails. The
  palette's commands on the active result's selection send a `GridCommand` (`store.ts`) to its
  grid, which runs it like its menu (`runGridCommand` in `ResultGrid.tsx`).
  In the SQL editor, completion, Ctrl+click and hover resolve a name with one reader of the
  statement (`src/sqlCompletion.ts`: `completeSql` and `resolveName`, tested by
  `scripts/sql-completion.test.mjs` and `scripts/sql-names.test.mjs`) over the catalog the tab
  read in the background (`useCompletionCatalog.ts`): never a query per key or hover, and a
  name that is not certainly one table or column shows nothing. They are hints: nothing is run
  or decided from them. Smoke test against the real app:
  `node apps/desktop/e2e/smoke.mjs` (after building the app and the CLI in one command, so
  nothing is built twice: `npx tauri build --no-bundle -- -p spelaion-desktop -p
  spelaion-cli -p spelaion-plugin-fetch --features spelaion-desktop/e2e`; a release build on
  16 GB needs `CARGO_BUILD_JOBS=3`, see `CONTRIBUTING.md`).
  It drives WebView2 over CDP, so the whole test runs on **Windows** only. The app opens the
  debugging port itself, but only in a build with the `e2e` feature, which only the smoke's
  builds turn on and never a release. `open_main_window` passes WebView2 the switches named in
  `SPELAION_E2E_WEBVIEW2_ARGUMENTS`, after wry's own (`src-tauri/src/webview_args.rs`). It
  accepts only the smoke's switches: the port (1024–65535), `--disable-features`, and three
  backgrounding flags. Anything else stops the start with a message. Gating it keeps a shipped
  build from getting around an administrator's policy on WebView2's switches.
  `scripts/e2e-feature.test.mjs` checks that the gate is in the source, that the feature is off
  by default, and that only `smoke-windows.yml` turns it on.
  WebView2's own `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` is ignored on GitHub's Windows runners
  (SDB-155). Its window is moved
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
  screenshots and the log (artifact `smoke-linux`). It and `ci.yml`'s Linux job install their
  Ubuntu packages in the background through `.github/workflows/system-packages.sh` (apt's
  retries and timeouts, then a 20-minute wait that fails saying whether apt failed or the mirror
  was still slow; tested by `system-packages.test.sh`, SDB-427).
  `.github/workflows/smoke-windows.yml` runs on the same events as the Linux pass, on
  `windows-latest`, and runs `smoke.mjs` with
  `SPELAION_E2E_UNTIL="SQL files: Ctrl+O opens one"`, because the next step, "direct
  PostgreSQL", is the first that needs `testing/docker`, and the runner has no Linux containers.
  `smoke.mjs` refuses an UNTIL that is not a step's text. A failure prints
  `FAILED at step "…"`, which the job's error names, and uploads `failure.png` and the log (artifact
  `smoke-windows`, SDB-155). A step before the cut that starts needing a server makes this job
  fail. In that case, move the step after the cut, or move the cut. Never skip the step. The
  job sets the runner's screen to 1920×1080 first: at its own 1024×768, Windows shrinks the
  app's 1400×900 window, and steps that measure room (the cell viewer's width) fail. The
  steps after the cut are still run by hand (AGENTS.md). macOS has neither (no
  WebDriver for WKWebView). `e2e/processes.mjs` closes windows and lists processes for both.
  Automatic updates (ADR-021): `src-tauri/src/update.rs` (Tauri's updater plugin, reading
  `latest.json` from the latest GitHub release, so releases are never pre-releases;
  `SPELAION_NO_UPDATES` switches them off, as the smoke test does). Update artifacts are signed only in the release workflow
  (`tauri.release.conf.json`, secret `TAURI_SIGNING_PRIVATE_KEY`); never commit a private key.
  Installers: `npx tauri build` (NSIS + MSI on Windows). Releases: pushing a tag `vX.Y.Z`
  runs `.github/workflows/release.yml` (installers for every OS, the `spelaiondb` CLI built once
  per platform by that platform's app job, which ships it inside the app and uploads its archive,
  SDB-129, so a change to the CLI's packaging goes in both app jobs, notes from
  `docs/releases/vX.Y.Z.md` plus every commit since the previous tag, from
  `scripts/release-notes.sh`, so commit subjects must read well in a changelog); the tag must
  match the version in `Cargo.toml` and `tauri.conf.json`. Pushing a branch `release-test/<x>`
  runs the same workflow as a rehearsal: the same installers and CLI archives, uploaded as the
  run's artifacts and checked by `scripts/check-release-plugins.sh` (each pinned plugin next to
  `spelaiondb` with its pinned hash). Its jobs only read the repository, never name a secret and
  build without update artifacts (`tauri.rehearsal.conf.json`); only the tagged jobs have
  `contents: write`. Pushing such a branch is a maintainer's step, never an agent's.
- `docs/code-signing-policy.md` (ADR-063, SignPath Foundation's terms) lists what is signed,
  who builds and approves, and every system the app contacts by itself. A change that makes
  the app or `spelaiondb` contact a new host, or contact one at another moment, updates its
  privacy section in the same pull request. `scripts/release-notes.sh` ends every release's
  notes with a link to it; keep that line.
- The side column (SDB-451): what it shows is `sidebarSection` alone (`store.ts`; `null` is
  hidden, `"mcp"` is the page that has no column), and `sidePanelToRestore` is only the panel
  that Ctrl+B brings back. `view.sidePanel` (Ctrl+B, `sidePanel.ts`) toggles it and moves the
  focus from a panel being hidden to that panel's activity-bar icon; the header's Hide side panel
  (`SidePanelButtons.tsx`) runs it. Showing a panel is otherwise its activity-bar icon, as before.
  Whether it is hidden and its width are personal preferences through the core
  (`get_side_panel` / `set_side_panel`, `spelaion-store`'s `side_panel.rs`, which owns the
  width limits); `App.tsx` restores them once the preferences are read and saves them as they
  change. It appears and disappears at once: no slide.
- Lifecycles in the app (ADR-060): `src-tauri` hands on the core's state types, and the page
  reads them from `bindings.ts`; it does not rebuild a state from events or derive it from
  flags. A state that exists only in the page (a dialog's phase, a value being loaded) is a
  discriminated union whose `switch` ends in a `never` check. The shared ones are in
  `src/dialogState.ts` (`Loaded<T>`, "preview, then apply", a job's phase; reducers tested by
  `scripts/dialog-state.test.mjs`) and their hooks in `src/useDialogState.ts`
  (`usePreviewThenApply`, `useJobPhase`, `confirmShown`): a new dialog uses them, not its own
  preview, error, busy or id flags. The hooks only show: what may run is still the core's
  check (ADR-047, ADR-008). The search panel's data search is one phase in `src/searchRun.ts`.
  Schema compare (ADR-070) is `SchemaCompareDialog.tsx`, opened from a schema's explorer menu
  and the palette (`src/schemaCompare.ts`): the menu item is disabled with the core's reason
  (`schema_compare_support`); `src-tauri/src/schema_compare.rs` opens a read-only session per
  side under one job, keeps the core's comparison by its job id until the dialog forgets it, and
  writes the script against the target's explorer session; the page holds only the report, and
  "Open script" opens it in a new editor tab on the target, never run.
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
  SDB-110: a run's progress changes its tab several times a second, so nothing that is always
  mounted subscribes to `s.tabs` itself. `App` and `TabBar` take the tabs' ids
  (`useShallow`), each `TabPanel` and `TabItem` reads its own tab (the item only the plain values
  it shows), and the explorer reads `connectedConnections` and a string of the active tab. A run
  update then re-renders only that tab's panel: measure a change here by counting renders with
  several tabs open, as SDB-110's pull request did.
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
  Copying a table to another connection (ADR-069) is `src-tauri/src/table_copy.rs` and
  `CopyDialog.tsx`: the dialog connects the target's explorer session as the explorer would
  (`openSession`), the plan opens a reading session of its own on the source (never the
  explorer's: ClickHouse's read barrier is a session setting that stays), and the run opens one
  for each side, a `Copy` job.
  Running work (exports, data searches, column statistics, imports, copies, backups, restores, file
  runs, connection tests, row loads, plugin installs) is one registry, `AppState.jobs`
  (`src-tauri/src/jobs.rs` over the core's `JobRegistry`, SDB-369): a job command takes a
  `job: Channel<JobTicket>` and sends the ticket (an id the registry made, and the kind) when
  the job starts; the page never makes an id (`src/jobs.ts`' `startJob` holds a Cancel pressed
  before the ticket arrives; `useJobPhase` and `searchRun.ts` keep the `Job`) and stops any job
  with `cancelJob(id, kind)`. A new job is one `jobs::start` call, held until it ends; no map
  to add or remove from.
- Split view (SDB-30): the editor area's layout is `useApp`'s `layout`, a `SplitLayout` union
  (`src/splitView.ts`: one tab, or two in panes `first` / `second`, side by side or one above the
  other, with the first pane's share), changed only through its transitions (`split`, `show`,
  `withoutTabs`, `resized`; tested by `scripts/split-view.test.mjs`). The focused pane is
  `activeTab`, nothing else: `setActiveTab` and `addTab` show the tab in the focused pane,
  `closeTabs` ends the split when a pane's tab closes. Each pane is an ordinary `TabPanel` (all
  stay mounted, as before), placed by `data-pane` in the workspace's grid, so a pane has its own
  session, results and connection row and nothing is shared between panes. The commands are
  `view.splitRight`, `view.splitDown`, `view.closeSplit` and `view.focusOtherPane`
  (`splitTab` and friends in `tabActions.ts`); none has default keys. The layout is not kept
  across restarts or a folder switch.
- Windows (SDB-30, ADR-083): every window is built by `build_window` in `lib.rs` (never a
  `WebviewWindowBuilder` elsewhere: WebView2 refuses a second webview with other switches) and
  registered in `src-tauri/src/windows.rs` (`Windows`: what each window shows, where its closing
  stands; its unit tests pin the transitions). The page reads its kind once in `main.tsx`
  (`windowContext.ts`); a results window renders `ResultsWindow.tsx` instead of `App`. Session
  keys come from `store.ts` (tabs) and `explorerSession` (explorer), both with `sessionPrefix()`:
  never write `meta:${id}` or `tab-${n}` by hand. `DesktopSession::window` is set from the
  calling window in `connect` / `connect_from_session`; `close_window_sessions` closes one
  window's sessions. An event meant for one window is sent with `emit_to` and heard with
  `events.x(getCurrentWebviewWindow()).listen` (a plain `listen` hears every target): SSH
  questions of `connect` and the tests (`AppInteraction::for_window`), `QuitRequested` and
  `LaunchRequested` (main only), `WindowCloseRequested`, `ResultsWindowClosed`. A tab and its
  results window talk through `resultsWindow.ts` (`sendMirror` / `serveRequests` on the tab's
  side, `ownerClient` on the other); a new callback of `Results` is a new `ResultsRequest`
  kind, handled in `EditorTab`'s bridge, whose `switch` ends in a `never` check. Only the main
  window restores and saves tabs, changes folder and checks for updates.
- The frontend build checks token use in `styles.css` and the canvas code with
  `scripts/check-ui-tokens.mjs`. Its counted baseline records existing declarations; remove
  each fixed occurrence from `scripts/ui-tokens-baseline.json` in the same change.
  `node scripts/check-ui-tokens.mjs --print-baseline` prints the current counts for comparison.
  Its only `!important` exemption is `[hidden] { display: none !important }`.
  Pull request CI compares the counts with the base commit so an entry can only decrease.
  `scripts/contrast.test.mjs` checks text, icon and focus token pairs in both themes, and the
  grid's translucent highlights blended over its rows.
- The grid's canvas reads its colors and fonts through `currentGridTheme()` in
  `ResultGrid.tsx`: one computed style per theme and device pixel ratio, cached under that key,
  never one per frame. A new color or font the canvas draws with goes into that object, read
  from a token defined in both themes.
