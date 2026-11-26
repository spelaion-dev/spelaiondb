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
  A Run the user presses (the editor's runs and Explain, by button, shortcut or palette) goes
  through `pressRun` in `EditorTab.tsx`, never straight to `run` (SDB-525): `src/runWait.ts`
  decides from what blocks the tab, read live (`blockNow`). The session opening, the database
  list after connecting, the history saving and a view being restored make one run wait, said
  in the toolbar with Don't run, and it runs once, the text as pressed; a folder change is said,
  never waited for; the session closing drops it with a word (ADR-028). Internal runs (a
  result's rerun, pages, refreshes) keep calling `run`. `scripts/run-wait.test.mjs` tests it.
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
  Saved tasks (ADR-089, SDB-33): `src-tauri/src/tasks.rs` only converts. `save_task` turns a
  dialog's request item (the same `ExportFileOptions`, `ImportRequestItem`, `BackupRequestItem`,
  `RestoreRequestItem`, `EmbeddedBackupItem` or file path it runs with) into the core's
  `TaskDocument` on the session's connection key (and, for a personal connection, the open
  folders' roots), with the session's resolved connection and typed password as the judges of
  `check_secrets`; a name another task has comes back as `nameTaken` and is replaced only with
  its id. `run_task` is a `Task` job over `spelaion_core::task::run_task` with
  `Questions::Ask`: a password or a yes comes back as the outcome, nothing recorded, and the
  page starts it again with the answer (`password` lives for that run only). The page is
  `TasksPage.tsx` (a page like the MCP page: `isPage` in `store.ts`), the dialogs' button is
  `SaveTaskDialog.tsx` (`useSaveTask`); the Windows smoke test's "saved tasks" step saves an
  export, runs it from the view and from `spelaiondb task run`, and reads both runs back.
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
  (`windowContext.ts`); a results window renders `ResultsWindow.tsx` instead of `App`. Each
  window reads its personal preferences once at start (`preferences.ts`, the shortcuts, the
  snippets, the display format, the update check). Every command that saves one, and the
  settings import, ends with `preferences_sync::announce` (`PreferencesChanged`: which kinds
  changed, SDB-533, SDB-535); each window reads those again from the core
  (`preferenceSync.ts`, an exhaustive switch over `PreferenceKind`), and a setting it is still
  saving keeps what it shows (`savedSetting.changedElsewhere`). A new personal preference a
  window keeps gets a `PreferenceKind` and an announce in its setter, or a window opened earlier
  shows the old value and saves it back. The side panel is left out: each window's layout.
  Session
  keys come from `store.ts` (tabs) and `explorerSession` (explorer), both with `sessionPrefix()`:
  never write `meta:…` or `tab-${n}` by hand. A PostgreSQL connection that opens several
  databases (ADR-074, SDB-504) has a database level in the explorer (`DatabaseNode` in
  `Sidebar.tsx`), from `connection_databases` (`explorer_databases.rs`: the core's list and
  each database's owner connection). Another database's nodes take `onDatabase(key, name)`
  (`connectionIdentity.ts`) as their identity, so `explorerSession` and `catalogKey` give that
  database its own session and catalog entries through `connectionTag`, while everything keyed
  by the connection (`sameConnection`, filters, commands) ignores it; a tab opened from them
  connects from that session. `connect_database` opens it through the core's
  `open_other_database`; its `DesktopSession` is marked `other_database` (a session made from
  it, after an edit, is refused rather than opened on the own database) and records its
  `explorer_parent`, so `disconnect` of the connection's explorer session closes it too.
  A tab's database (SDB-505) is `Tab.database` (null for the connection's own), set from the
  source in `connectTabFromExplorer` and kept in `open_tabs.database`; every connect of an
  editor tab goes through `openTabSession` (`connect.ts`), which opens its connection then moves
  it to its database with `change_tab_database` (the core's `change_session_database`: refused
  mid-run, then `open_other_database`), or leaves it disconnected, never on another database.
  Bookmarks and table filters carry the database (`BookmarkItem.database`, the filters'
  commands' `database`), empty in the store for the connection's own. What is opened from
  another database acts on it (SDB-506): `openBackup`'s `database` (the row's session, the
  dialog's name), `addDiagramTab`'s `database` (from `sourceDatabase`), the ER diagram and table
  details connect through `openTabSession`, the data search's choices are catalog tags, and
  `PrivilegesDialog` takes the connection's `databases` and reads and writes grants in the
  chosen database's explorer session (`connect_database` is a no-op when it is already open). `DesktopSession::window` is set from the
  calling window in `connect` / `connect_from_session`; `close_window_sessions` closes one
  window's sessions. An event meant for one window is sent with `emit_to` and heard with
  `events.x(getCurrentWebviewWindow()).listen` (a plain `listen` hears every target): SSH
  questions (`AppInteraction::for_window`, the only constructor: `connect` and the tests take
  the calling window, a job the window of its source session's `DesktopSession::window`, SDB-522), `QuitRequested` and
  `LaunchRequested` (main only), `WindowCloseRequested`, `ResultsWindowClosed`. A tab and its
  results window talk through `resultsWindow.ts` (`sendMirror` / `serveRequests` on the tab's
  side, `ownerClient` on the other); a new callback of `Results` is a new `ResultsRequest`
  kind, handled in `EditorTab`'s bridge, whose `switch` ends in a `never` check. Only the main
  window restores and saves tabs, changes folder and checks for updates.
- A connection crosses to and from the page as its identity (ADR-075, SDB-495): every command
  that takes a connection takes a `ConnectionKeyItem` (its folder and its id), never the id's
  text, and reads it only through `connection_in` in `lib.rs`, which refuses a pair whose
  folder is not the connection's (or that names no connection). `ConnectionItem` carries the
  folder; bookmarks carry the identity. In the page the pair is a `ConnectionKey`
  (`src/connectionIdentity.ts`, pure and unit-tested; `connectionKey.ts` adds
  `connectionKey(id)`, the lookup for what still holds a bare id). The explorer, its sessions,
  `explorerActions.ts`, the catalog (`catalogKey`), the completion cache, and the dialogs and
  pickers the explorer opens all hold it: compare two with `sameConnection` (never `===` on
  objects, never `includes` on an array of them), and key text (maps, React keys, a
  `<select>`'s values, session keys) with `connectionTag`, read back only with
  `connectionOfTag`. An effect depends on the tag, not on the object. The explorer's window
  events carry the pair: send them with `schemasRefreshed`, `refreshExplorer` and
  `explorerSessionClosed` (`connect.ts`), never by hand. A tab holds both halves
  (`connectionId` and `connectionFolder`, ADR-075 point 7, SDB-502): read its identity with
  `tabConnection`, compare with `tabOnConnection`, and after an await check it with
  `stillOnConnection`; whatever changes a tab's connection sets both. The tab's connection
  picker's values are tags. The history and saved queries carry the stored `identity`
  (`ConnectionKeyItem::stored`: the store's scope and id) and reopen on that folder's
  connection; `add_connection` returns the new identity. Only a launch and the MCP page still
  look a bare id up with `connectionKey(id)`. The metadata search (`dbSearch.ts`) gets each
  connection as its tag.
- Open folders (ADR-075, SDB-500): `AppState::folders` (`src-tauri/src/open_folders.rs`) is
  the ordered list of open folders, at most `MAX_FOLDERS` (ten), each with its own `Config`
  (that folder alone with the personal layer) and its canonical root; the same root is never
  open twice. `AppState::config()` is the folder the page shows (the last one opened, or the
  personal layer alone), `open_configs()` every open folder's: a personal connection is
  resolved with `Resolver::with_open_folders` and its definition with
  `connection_definition_open` over them, a folder's connection only ever with its own.
  `reload` loads each folder again on its own and installs it only into the same opening of
  that folder. Each `DesktopSession` records its connection's folder; `close_folder` closes
  that folder's sessions and no other (`open_folders::sessions_of`), personal ones stay.
  Commands: `add_folder` (closes nothing; returns the folder's root and whether it was not
  open before), `close_folder`, `list_folders` (each with its trust), `trust_workspace(root)`
  for any open folder (as opened or its canonical root), and `open_workspace`, which closes
  every other open folder first (the page no longer calls it; kept for raw callers). In the
  page (SDB-502), `folders.ts`'s `addFolder` and `closeFolder` are the only ways folders
  change: from the folder menu, a recent folder, the palette, a launch, an explorer group's
  Close. Both run inside `tabPersistence.ts`'s `whileFoldersChange`, which writes the tabs
  first and sends none until the tabs on screen match the open folders again: adding puts the
  folder's kept tabs back (`restore_tabs_for_workspace(folder)`, each at its kept
  `position`), closing takes its tabs out without letting Ctrl+Shift+T reopen them. Closing
  asks only about that folder's tabs (`confirmLeaving`'s `among`). The store's `openFolders`
  is refreshed with the connections (`refreshConnections`). The explorer groups by folder only
  with two or more open; a connection's folder-tree keys and drops carry its scope (its
  folder's root, `""` for personal: `scopeOf`, `data-drop-scope`), so it moves only among its
  own folder's folders. `folderNames.ts` (pure, tested) names folders and, where two share a
  name, connections in pickers (`useConnectionName`). `SaveInChoices.tsx` is the folder choice
  of the add and import forms. Adding or closing
  a folder is a switch of `AppState::folder` (SDB-377), so a connection started before lands
  nowhere. The open folders are remembered in `desktop-open-folders.json` and opened again at
  start (a launch that names a folder opens that one); `desktop-last-workspace.txt` is read
  once, to migrate. The unit tests of `open_folders.rs` pin the limit, the roots, the order,
  what is remembered and what a reload may replace. Tabs (SDB-501): the page sends the whole
  window with `save_tabs`, each tab with its connection's folder (`OpenTabItem::folder`), and
  the core keeps each under that folder or the window's key (`tab_scope`, `window_sets`);
  `restore_tabs_for_workspace` restores every open folder's and the window's, in the window's
  order. So a tab on a personal connection, or none, stays across a change of folder, and the
  page never names a folder key itself. History and saved queries list every open folder's
  (`library.rs`'s `open_scopes`). Every command that takes a connection finds it with
  `AppState::connection_config` (SDB-502): the configuration of the open folder its identity
  names (the shown one for a personal connection), then `connection_in`; never
  `connection_in(&state.config())`, which only knows the shown folder. Where a new or imported
  repository connection goes is `folder_config(repository)` (`add_connection`'s and the
  import's `repository`, the shown folder when none). `list_connections` lists every open
  folder's connections and the personal ones once (`listed_connections`), each with its own
  folder's types, and a connection's sessions are told apart by folder as well as id. An
  "untrusted" outcome (`ConnectOutcome`, `ConnectionTestOutcome`, `EditorAccessOutcome`) names
  the folder it is about (`root`), which the page trusts: not always the shown one.
- SQL notebooks (ADR-087, SDB-36): a notebook tab is a file tab (`tab.file`, `tab.sql`) with
  `tab.notebook` (`store.ts`): `sql` is the notebook's text as the core writes it from the cells
  (`notebook_text`, `notebook.ts`'s `syncNotebookText`, after each change and before every save),
  so saving, the changed-on-disk check, the unsaved mark, closing's question and kept tabs are
  `sqlFiles.ts`'s and ADR-036's; an opened or restored notebook reads its cells from its text
  (`parse_notebook`) and is not written again until a cell changes. `.md` paths open as notebooks
  (`openFilePath`). `NotebookTab.tsx` runs a cell as the editor runs a script: `expand_notebook_cell`
  (the core's CTE expansion with the session's dialect), `script_risks` and the confirmation,
  then `run_notebook_cell` (`notebook.rs`), which is `start_script` with the cell's results key
  (`cell_results_key`), so `check_run`, the history and the run options are the editor's; never
  run a cell's text any other way. Each cell's results are freed with the tab (`close_editor`
  frees its cells' keys too) or when the cell goes (`release_notebook_cell`). Markdown is drawn
  from `notebook_markdown`'s blocks as React text; links and images are never followed or
  fetched. The HTML export (`export_notebook_html`) sends result ids and a shown chart's SVG,
  never rows. A command that acts on an editor tab excludes notebooks (`editorTab` in
  `commands.ts` and the other kind checks); `fileTab` is the editor's or a notebook's.
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
