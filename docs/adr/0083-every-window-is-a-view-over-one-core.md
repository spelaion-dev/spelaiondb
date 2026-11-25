# ADR-083: Every window is a view over one core

- **Status:** Accepted (implemented)
- **Date:** 2026-11-25
- **Builds on:** ADR-002 (the core holds the rules; the app is an adapter), ADR-004 (no
  daemon: one process), ADR-010 (results live in the core and are read in windows of rows),
  ADR-031 (closing asks about open transactions), ADR-036 (the main window's tabs come back),
  ADR-060 (a lifecycle is one explicit state, owned in one place)

## Context
SDB-30 asks for two things beyond the split view (which needs no decision: two tabs of one
window side by side): showing a tab's results in a window of their own, on a second monitor,
and opening a connection in a window of its own. Its constraints: each window is another view
over the same core; sessions and results stay in the core, never duplicated per window; two
windows show the same result without fetching it twice; closing one window does not touch the
other's sessions.

Until now the app had one window. Several things silently assumed it:

- **Session keys are chosen by the page** (`tab-3`, `meta:shop`). A second page counting tabs
  from 1 would name the same sessions as the first.
- **Events are broadcast.** A session's loss, its transaction, a run's state, an SSH question,
  the quit request and a second launch's files reach every page. With two pages, both would
  answer an SSH prompt, and a second window would quit the app or open the launched files.
- **Closing the window is quitting.** The quit question covers the main window's tabs, and the
  core closes every session.
- **The page's state of a run** (the results' metadata, the messages, the run's status) lives
  in the page that ran it, by design: rows never do (ADR-010).

## Decision
**One process, one core state, several windows. A window is a view: it owns nothing the core
keeps, only the page state it already had. The desktop adapter keeps a registry of the open
windows, and each session records which window opened it.**

1. **The registry** (`src-tauri/src/windows.rs`) holds one entry per window: what it shows (the
   main window, a connection's window, or the results of one tab of another window) and where
   its closing stands (open, or its page asked to close, with the quit that waits for the answer
   if any). Its transitions are written there and tested there (ADR-060). The page asks the core
   what its window shows (`window_context`) before its first render, and never guesses it.
2. **Session keys stay the page's, but never collide.** A window other than the main one
   prefixes every key it makes with its label (`connection-2/tab-1`, `connection-2/meta:shop`).
   The core keeps keys opaque, as before. Each session records the label of the window whose
   page opened it (`connect` and `connect_from_session` read it from Tauri, not from the page).
3. **What is meant for one window goes to that window only.** SSH questions go to the window
   that asked to connect or test; the quit request and a launch's files go to the main window;
   a connection window's close request goes to it; "your results window closed" goes to the
   tab's window. Each page listens to those on its own window. Events about one session stay
   broadcast: keys never repeat, so a page ignores another window's sessions as it ignores a
   closed tab's.
4. **Closing a window closes the sessions it opened, and only those**, through the same path
   as closing their tabs, and frees their results. A connection window's page asks first, about
   its own tabs only: open transactions, running queries, row edits not applied, and SQL that
   is kept nowhere else (its tabs are not restored, unlike the main window's, ADR-036). A second
   close request while its page has not answered closes it, as the main window's second request
   quits. A results window owns nothing and closes at once; it goes with the window it belongs
   to.
5. **The main window is the app.** Closing it quits, as before; first each connection window is
   asked to close, one at a time, each with its own question; one that stays open stops
   quitting. The open folder is the whole app's, so only the main window changes it, and only
   the main window checks for updates.
6. **A results window reads the same results.** The rows stay in the core's store; the results
   window asks for windows of rows by result id, exactly as the tab's grid does, so nothing is
   run or fetched again. What the tab's page knows of its run (metadata, never a row) goes to
   the results window on every change, and the results window's requests (a sort, a filter, the
   next page, Cancel) go back to the tab, whose own handlers carry them out. The tab's window
   stays the only owner of its run; the results window only shows it and asks. Row edits stay
   in the tab: the results window's grid is read-only. While the results are in their own
   window, the tab shows its editor and one line with the run's status, Show results window
   and Re-attach results.
7. **A connection's window** is the same page as the main window, offering one connection:
   its explorer, pickers and palette list only it; it starts with a new query on it; nothing it
   opens is kept for the next start; it cannot change the folder.

## Consequences
- Every window is built by one function (`build_window`), so each gets the same WebView2
  switches (WebView2 refuses a second webview with other options over the same data folder),
  Linux's clipboard access, the smoke test's place and the interface size.
- `capabilities/default.json` names the windows by pattern (`connection-*`, `results-*`): they
  get the same permissions as the main one, no more.
- A job that is not started by a window's page yet (backups, imports, a table copy's reading
  session) still broadcasts its SSH questions; with more than one window open, each would show
  it. Sending those to the window that started them too is SDB-522.
- "Disconnect all" on a connection acts on the window it is chosen in: its tabs and its
  explorer. Another window's sessions on that connection stay, as closing one window leaves
  another's.

## Alternatives
- **A window per webview process with its own core** (a second app instance): two result
  stores, two sets of sessions and tunnels; the constraint forbids it.
- **The core scoping session keys by window itself**, by taking the calling window in every
  command: about a hundred commands would change to say what the prefix already says, and the
  CLI and the MCP have no windows.
- **The results window owning a copy of the run's state**: two owners of one state that both
  change (rows arriving, a sort), which ADR-060 rules out. One owner and a view that asks it
  keeps one place for every transition.
- **Peers, the app quitting with its last window**: the main window holds the folder, the
  launch channel and the restored tabs; making any window able to outlive it would move all of
  that. Kept simple until someone needs it.

## What would change this
- If a results window needs to edit rows, the edits must keep living in the core with the tab's
  session (ADR-044), and the results window would ask the tab for them as it asks for a sort.
- If people want their connection windows back after a restart, the tabs keeper (ADR-036) would
  keep tabs per window, with the window's connection.
