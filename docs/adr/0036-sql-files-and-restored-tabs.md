# ADR-036: SQL files in editor tabs, and open tabs that survive a restart

- **Status:** Accepted (implemented)
- **Date:** 2026-11-08
- **Builds on:** ADR-011 (local storage), ADR-025 (personal state per workspace), ADR-028 (nothing
  is re-run after reconnecting), ADR-031 (transactions), ADR-035 (script runs)

## Context
People keep their queries in `.sql` files, often in the same git repository as `.spelaion/`.
The editor only had saved queries (snippets) in the local store, and quitting lost every open
tab. A database client is used all day: a restart, an update or a crash must not cost the text
of the tabs that were open. At the same time, restoring a session must never turn into running
something: AGENTS.md says nothing is re-run after reconnecting, and a restored tab is a
reconnection in all but name.

## Decision
1. **File access goes through the core** (`spelaion-core::sql_file`). React never reads or
   writes a file: it sends the path the user picked in the system's open or save dialog, as
   exports already do, and the core reads or writes it. This is the same boundary as exports:
   a script in the webview that could call these commands could already call every other one.
   - **Reading** refuses files over 16 MiB and files that are not text (a NUL byte without a
     UTF-16 byte order mark).
   - **Encodings:** UTF-8 with or without a BOM, UTF-16 LE and BE (with their BOM), and, for
     files that are not valid UTF-8 and have no BOM, Windows-1252 (the "ANSI" code page of
     Western Windows, which also reads every Latin-1 file; its five unassigned bytes map to the
     matching C1 controls, as WHATWG does, so every byte round-trips). A file is written back
     in the encoding it was read in; a character that Windows-1252 cannot hold makes the save
     fail with that character named, and the app offers to save as UTF-8 instead. Other
     encodings (Shift-JIS, GB18030, UTF-16 without a BOM…) are not detected: they open as
     Windows-1252 or are refused as binary. New files are UTF-8 without a BOM.
   - **Line endings:** the editor works with `\n`. The core notes the file's line ending
     (`\n`, `\r\n` or `\r`, whichever is most common; `\n` on a tie or with no line break) and
     writes every line break with it. A file with mixed line endings is therefore written with
     the most common one. New files use the platform's (`\r\n` on Windows).
   - **Writes are atomic:** the bytes go to a temporary file next to the target, are flushed to
     disk, and replace the target with a rename, so a crash leaves the old file or the new one,
     never half of one. On Unix the temporary file takes the target's permissions first; a
     symbolic link is followed, so the link stays a link.
   - **Changed on disk:** every read and save returns the SHA-256 of the file's bytes. A save
     sends the hash it last saw; the core reads the file again just before writing and refuses
     with `ChangedOnDisk` (or `DeletedOnDisk`) if it differs. The app then asks: overwrite,
     reload from disk, or cancel. A content hash, not the modification time, so a
     `git checkout` that puts back the same bytes is not a conflict. A change between that
     check and the rename is not detected; the window is a few milliseconds.
2. **Tabs, files and saving.** An editor tab is a *console* (text only in SpelaionDB) or a
   *file* tab (text from a `.sql` file). Ctrl+O opens a file; Ctrl+S saves a file tab (and, on a
   console, still saves the query by name, as before); Ctrl+Shift+S saves any editor tab as a
   file, which turns a console into a file tab. A file tab whose text differs from what was last
   read or saved shows an unsaved marker (●) and its title is the file's name. Closing it asks:
   save, don't save, or cancel. Opening a file that is already open in a tab shows that tab.
3. **Open tabs are persisted per workspace** in the local store (`open_tabs`, one row per tab,
   migration 10), keyed like the explorer's filters (ADR-025): the repository's canonical root,
   or the empty key without a folder. Each tab keeps its kind, title, text, connection id,
   pinned flag, order, its own run options and, for a file tab, the path, the hash it last saw,
   its encoding and line ending, and whether it has unsaved changes. **Never** results, secrets,
   passwords, a transaction or any session state: a restored tab has none of them to restore.
   Table data and metadata tabs are not persisted (they are views of the database, opened again
   from the explorer). Switching folders saves the tabs under the old folder and brings back the
   new folder's: tabs belong to the folder they were opened in.
4. **How often it saves.** The app sends the tabs to the core at most every 250 ms while they
   change. The core's `TabKeeper` writes them at most 1 s after the first change it has not yet
   written (a deadline, not a debounce that typing keeps pushing back), in one transaction that
   replaces the workspace's rows. A crash therefore loses at most the last ~1.25 s of edits;
   quitting writes what the core already has at once. Unsaved text of a file tab is restored as
   unsaved, so quitting does not ask about unsaved files: nothing is lost by quitting.
5. **Restoring never runs anything.** At startup and after switching folders the app asks the
   core for the workspace's tabs (`restore_tabs`). Every tab comes back **disconnected**, with
   its text and nothing else. The core checks each one:
   - a connection that no longer exists leaves the tab with its text and no connection, and a
     note that names the missing connection;
   - a file tab without unsaved changes is read again from disk (someone may have pulled a new
     version); if the file is gone or unreadable, the saved text comes back as unsaved, with a
     note;
   - a file tab with unsaved changes keeps its text; if the file changed on disk since, a note
     says so, and saving will ask (point 1).
6. **"Reconnect at startup" is a personal setting, off by default** (Settings › Editor). When it
   is on, after restoring, the app connects each restored tab that has a connection, one at a
   time, through the ordinary connect path: it may ask for a password, for trust in a repository
   or for a changed connection to be accepted, exactly as a click would. It only connects; it
   never runs, and it never opens a transaction. A tab whose connection fails stays
   disconnected with the error.
7. **Recent files and reopening closed tabs** are different lists and do not overlap:
   - *Reopen closed tab* (Ctrl+Shift+T, SDB-4) is this run's memory of closed tabs of any kind,
     in the order they were closed; it is not persisted and is cleared when the folder changes.
     A closed file tab comes back as it was closed: its file, its text and, if it had unsaved
     changes the user chose not to save, those changes, still marked unsaved (a way back from
     "don't save"); saving it checks the file on disk as any save does.
   - *Recent files* is the persistent, personal list of the last 20 files opened or saved, in
     every folder (a file is not tied to a workspace), newest first. Opening one that is already
     open shows its tab; one that no longer exists is shown as missing and can be removed.
8. **Files inside the repository** sit next to `.spelaion/` and change nothing about trust:
   opening a file is reading text. Running it goes through the usual splitting, classification,
   guard and confirmations of the tab's connection (ADR-008, ADR-029), like any typed text.

## Alternatives
- **Dialogs and file I/O in the webview** (Tauri's fs plugin): less code, but encodings, atomic
  writes and the conflict check would live in React, against "security and logic in the core".
- **Only Rust-side dialogs, and a list of granted paths:** stricter, but restoring needs paths
  from the store anyway, the smoke test cannot drive native dialogs, and it adds nothing while
  any command can already run SQL. Revisit if the app ever loads untrusted web content.
- **Persist the tabs as one JSON document:** simpler to write, but rows with check constraints
  are validated by SQLite and migrate like the rest of the store.
- **Detect any encoding (`encoding_rs` + `chardetng`):** two more dependencies for guesses that
  are often wrong on short SQL files; UTF-8, UTF-16 and Windows-1252 cover what people use.
- **Warn about unsaved files when quitting:** not needed, since they come back as unsaved.

## What would change this
- Users with files in other legacy code pages (CP-1251, Shift-JIS): an explicit "reopen with
  encoding" choice, rather than more guessing.
- A request to keep closed tabs across restarts: then "reopen closed" would persist too, and the
  two lists would need merging rules.
