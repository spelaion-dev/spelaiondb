# ADR-075: A window may hold several folders: each loaded, trusted and resolved on its own, a connection identified by its folder and its id, and the MCP server still serving one folder

- **Status:** Accepted (not implemented)
- **Date:** 2026-11-24
- **Builds on:** ADR-004 (no daemon), ADR-009 (configuration layers, keychain names), ADR-022
  (trust per repository, pinned to content), ADR-029 (connection types: safeguards only add
  up), ADR-033 (the MCP page), ADR-074 (a database is not part of a connection's identity)
- **Amends:** ADR-036 (point 3: a tab belongs to the folder of its connection, not to the
  folder that was open)
- **Decides:** SDB-401

## Context
The app shows one repository folder at a time, with the personal connections. People who work
on several services change folder all day, and changing closes every session, the personal
ones included, after a question. ADR-022 left "several repositories open at once" out, to be
revisited if people really work with several. DBeaver shows projects side by side; VS Code has
several folders in a window and several windows.

What "one folder" is in the code, checked on `dev`:

- **The configuration holds one root.** `spelaion_config::Config` has `workspace_root:
  Option<PathBuf>` and is loaded from one starting directory (`load(start, user_dir)`): at most
  the repository's layer and the user's. A `ConnectionId` is a layer (`Workspace` or `User`) and
  a path; its text is `prod/orders`, or `~/local` for a personal one, and the desktop parses
  that text back (`parse_id`). **An id does not say which repository it is from.**
- **The desktop holds one folder** (`AppState.workspace`, one `Config`). `open_workspace` closes
  every session before loading the new one. There is one window (`main`), one app per personal
  folder (`instance.rs`), and a launch that names a folder switches to it after the same
  question (`launch.rs` refuses a second folder in one launch).
- **What is stored is already kept per folder.** The local store's tables are keyed by
  `workspace` (the repository's canonical root, or empty for personal connections) and the
  connection id: history, the history switch, saved queries, explorer filters and bookmarks,
  editor variables, table filters, the agents' read-only override (ADR-033) and open tabs
  (ADR-036). Keychain entries carry the root (`repo:<root>/<id>/password`, ADR-009). The trust
  store keeps trusted roots and pins by root (`TrustStore`), although its functions take "the
  open repository" from the one `Config`. **The audit log is the exception**: it records the
  connection id alone.
- **A personal connection takes things from the open folder today.** The resolver gives every
  connection, personal ones too, the open repository's team types (`ConnectionTypes::load`), and
  every SSH tunnel its team `known_hosts`; a tunnel is looked up by name in the connection's
  layer first and then in the other (`Config::tunnel_for`).
- **The tunnel pool's key** is the layer, the tunnel's name, its settings and its hops
  (`open.rs`): no folder.
- **The MCP server serves one folder**: `spelaiondb mcp --workspace <dir>`, or the directory it
  is started in, read again on every call. The entry the MCP page writes into AI clients names
  no folder (ADR-033). It never knows what the app has open (ADR-004).
- **The page** builds an explorer with two groups (repository and personal) and uses the bare
  id as the key of rows, of the explorer's sessions (`meta:<id>`) and of its caches.

## Decision
1. **Yes: one window holds a list of folders**, at most ten, with the personal connections
   shown once. Opening a folder adds it and closes nothing. Closing a folder asks what
   switching asks today and closes only that folder's sessions; sessions on personal
   connections stay.
2. **Each folder is loaded exactly as it is today: alone, with the personal layer.** One
   `Config` per folder; no `Config`, and no resolver, ever sees two repositories. Trust, its
   pins and its reviews are that folder's, by its canonical root, as the trust store already
   keeps them: trusting, reviewing or breaking one folder changes nothing in another. Each
   folder shows its own "not trusted" and "changed" state.
3. **A connection is identified by its folder and its id**: the repository's canonical root,
   or none for a personal connection, and the id as it is today. It is one type of the core's,
   which every desktop command takes and the generated bindings carry; nothing parses a folder
   out of an id, and an id's text never grows a path. The sessions, caches and rows the page
   keys by a bare id are keyed by the pair. With ADR-074, what belongs to one database of a
   PostgreSQL connection carries the database next to this pair, and is not part of it.
4. **A folder's connection sees its own folder and the personal layer, never another folder.**
   Its type comes from its folder's `connection-types.toml`, the user's and the built-in ones
   (ADR-029, unchanged); its tunnels check its folder's `known_hosts`; a tunnel it names is its
   folder's, then a personal one. This is what one `Config` per folder already does.
5. **A personal connection sees every open folder as it sees the one open folder today, and
   only where that tightens.** Its type's safeguards add up across every open, trusted folder
   that defines that type (looks come from the user's definition, then the first open folder
   that has one, then the built-in one). Every open, trusted folder's `known_hosts` is checked
   before the user's, and a different key in any of them blocks. A folder whose types file does
   not load, or changed since it was trusted, refuses the typed personal connections until it
   is fixed or reviewed, as today. A tunnel it names is a personal one first; failing that, the
   one open folder that has a tunnel of that name, and it is refused, naming the folders, when
   several do. Nothing from a folder can take a safeguard away from a personal connection.
6. **The tunnel pool's key gains the folder**, so two folders' tunnels of the same name and
   settings are never one chain verified against the other folder's host keys.
7. **A tab belongs to the folder of its connection** (amends ADR-036's point 3). It is saved
   under that folder, closed and kept when the folder is closed, and restored when the folder
   is open again; its connection is looked up in its own folder only, so it never comes back on
   another folder's connection of the same id. A tab on a personal connection, or with none,
   belongs to the window: saved under the empty key, it stays whatever folders open or close.
   Tabs saved before this, under the folder that was open, are restored with that folder and
   saved again under the new rule. A saved query is kept under the folder of the tab's
   connection, as now; history and saved queries list every open folder's and the personal
   ones.
8. **The explorer** has the personal group and one group per folder, named after the folder,
   with its parent when two have the same name. Where a connection's name could be either of
   two, the tab and the pickers show its folder. Adding or importing a connection asks which
   folder, or personal; a pasted one is personal (ADR-030).
9. **The MCP server does not change: one folder per server**, the one it is started in or
   given, and the personal connections. It never serves "the folders the window has open": that
   is the app's state, which the server does not read (ADR-004), and it would expose a
   repository to an agent because a window happened to have it open. So `list_connections`
   never holds two connections of the same id. The MCP page lists every open folder's
   connections by folder, each with what agents can do there as its own folder decides; the
   per-connection override is already kept per folder. **The audit log records the folder** of
   each new entry, so the activity of two folders' `prod/orders` can be told apart.
10. **Outside the app.** One app per personal folder, as now. A launch that names a folder
    adds it to the window and asks nothing, since it closes nothing. The open folders are
    remembered and opened again at the next start, each trusted or not as it was. `spelaiondb
    check`, `trust` and `connections` keep taking one folder.
11. **In two steps.** First the identity of point 3, with one folder and no change in
    behaviour: it is the part that touches every command and the page, and ADR-074 needs it
    too. Then several folders.

## Alternatives
- **One window per folder** (the most reasonable alternative; what DataGrip does, and one of
  VS Code's two ways). Each window is today's model, so identity, trust and what a personal
  connection takes from the open folder stay as they are. But the desktop's state is one
  folder and one map of sessions for the whole process, keyed by ids the page makes up, so it
  would have to be divided by window just as it has to be divided by folder here; every window
  is another WebView; and two folders are never in one explorer or one row of tabs.
- **A `Config` with several roots.** Every path of the resolver would have to answer "which
  root" for types, host keys, tunnels, paths and keychain names, and one wrong answer uses one
  team's host keys or safeguards for another team's connection. One `Config` per folder cannot
  make that mistake.
- **The folder inside the id's text** (`service-a:prod/orders`). Ids are stored, shown to
  agents and compared as text; a folder's name is not unique and its path is not something to
  parse back out of a string.
- **The MCP server serves every open folder.** See point 9.
- **Personal connections stop taking anything from folders.** Simpler, and a repository could
  then never affect a personal connection. But a personal connection whose type a team
  tightened would lose that safeguard: a decision of its own, not a side effect of this one.
- **No: recent folders are enough.** They make switching quick (v0.12), but switching still
  closes every session.

## What would change this
- If the first step shows that carrying the folder through the desktop's commands is a rewrite
  rather than a change of key, one window per folder is taken instead, before anything of the
  second step is built.
- If what a personal connection takes from the open folders proves confusing with several of
  them, the personal layer becomes self-contained, in an ADR that says which safeguards that
  gives up.
- If agents must work across folders, `--workspace` given more than once, with ids that say
  their folder, in an ADR of its own.

## Work items if accepted
- A connection's identity is its folder and its id: the core's type, the desktop's commands,
  the bindings and the page's keys; the audit log records the folder. One folder still (L).
- The desktop holds several folders: one `Config` each, open, add and close, sessions closed
  per folder, the remembered set, launches add (M).
- Personal connections with several folders open: types that only add up, every folder's host
  keys, the ambiguous tunnel refused, the tunnel pool's key; with tests that try to get around
  each (M; `resolve.rs` and `types.rs` need an owner).
- The explorer's groups, each folder's trust state, and where a new or imported connection
  goes (M).
- Tabs belong to their connection's folder; history and saved queries over the open folders
  (M).
- The MCP page by folder, and its activity by folder (S).
