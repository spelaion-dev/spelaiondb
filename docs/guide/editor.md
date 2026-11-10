# Running SQL

For MySQL, MariaDB and SQL Server, the database selector beside the connection chooses the
default database for that query tab. It shows the current database after `USE` statements too; other
tabs and the saved connection are unchanged. Connect first to load the databases available
to the user. Double-click a MySQL or MariaDB database in the connected explorer to open a
query tab already connected to it. Queries and table tabs opened from the connected explorer
use their own sessions, without asking again for the password already provided there.

**Ctrl+Enter** runs the selection or, without one, the statement under the cursor, which is
highlighted for a moment. The core finds it with the same dialect splitter that runs scripts
(quotes, comments, dollar-quoted bodies and MySQL `DELIMITER` included), so what is
highlighted is exactly what runs. **Run** in the editor's side toolbar does the same.
**Ctrl+Shift+Enter** or **Run script** runs the whole script,
statement by statement, stopping at the first error. **Shift+Alt+F** or **Format** formats
the selection or the whole script in the connection's dialect (one undo step).

`:name` placeholders (like those in generated SQL) ask for their values before running; the
same name gets the same value everywhere, and the tab remembers them for the next run. The
core writes them in as literals with the engine's quoting: numbers, `NULL`, `TRUE` and
`FALSE` as they are, anything else as text. Strings, comments and PostgreSQL `::casts` are
never mistaken for placeholders.

Before anything runs, the core looks for statements that are easy to regret: `DELETE` or
`UPDATE` without `WHERE` (also inside a `WITH`), `DROP` and `TRUNCATE`, and, on connections
whose environment is `prod`/`production`, every statement that is not a read. They are listed
in a confirmation dialog where **Cancel** has the focus; nothing runs until you choose
**Run anyway**. This is a safety net against slips; read-only mode remains the barrier for
connections that must never change.

## Keyboard shortcuts

| Keys | Action |
| --- | --- |
| Ctrl+Enter | Run the selection or the statement under the cursor |
| Ctrl+Shift+Enter | Run the whole script |
| Shift+Alt+F | Format the selection or the script |
| Ctrl+S | Save the file of a file tab; in a console, save the SQL as a named query |
| Ctrl+O | Open a `.sql` file in a new tab |
| Ctrl+Shift+S | Save the tab as a `.sql` file (a console becomes a file tab) |
| Ctrl+T / Ctrl+W | New query tab / close the tab (its connection and results are freed) |
| Ctrl+Shift+T | Reopen the last closed tab with its SQL, connection and name (disconnected) |
| Ctrl+Tab, Ctrl+Shift+Tab | Switch to a recently used tab: keep Ctrl down, press Tab to move, release to switch |
| Ctrl+PageDown / Ctrl+PageUp | Next / previous tab in the bar |
| Ctrl+Shift+PageDown / PageUp | Move the tab right / left |
| Ctrl+F2 (F2 on a focused tab) | Rename the tab |
| Ctrl+Alt+D / Ctrl+Alt+P | Duplicate / pin or unpin the tab |
| Ctrl+Alt+W / Ctrl+Alt+R / Ctrl+Shift+W | Close the other tabs / the tabs to the right / all tabs |
| Ctrl+Alt+M | Maximize or restore the results panel |
| Ctrl+, | Settings |
| Ctrl+C, Ctrl+Shift+C | Copy the grid selection (with headers) |
| Enter, Esc | Open / close the cell viewer |
| Ctrl+Shift+J | Jump to a column of the result by name |

## Tabs

Right-click a tab (or press Shift+F10 on it) to rename, duplicate, pin or close it, or to
close the others, the ones to its right or all of them. Double-click a tab to rename it, drag
it to reorder, and middle-click it to close it. A spinner on the tab shows that its query is
running.

Closing tabs asks once, listing every tab whose transaction the server would roll back and
every tab whose running query would be cancelled. Pinned tabs sort first and stay open when
you close the others, the ones to the right or all; their × becomes an unpin button so a stray
click never closes one, but closing that tab itself (Ctrl+W or **Close** in its menu) does.
A reopened tab brings back its SQL, connection and name, never its results or session: it
starts disconnected and runs nothing until you do.

## SQL files and tabs that survive a restart

**Ctrl+O** (or the folder button next to **+**) opens a `.sql` file in a tab named after it;
the same button lists the recent files. **Ctrl+S** saves it, keeping its encoding (UTF-8 with
or without a BOM, UTF-16, Windows-1252) and its line endings; **Ctrl+Shift+S** saves any tab as
a file. A ● on the tab marks unsaved changes, and closing it asks to save them. If the file
changed on disk since it was opened (a `git pull`, another editor), saving asks before
overwriting it. Files are written whole or not at all (a temporary file, then a rename).

Open tabs, consoles and files alike, come back after a restart or a crash (losing at most the
last second of typing), with their text, name and connection, and unsaved changes still
unsaved. They come back **disconnected** and never with their results: nothing runs on its
own. A tab whose connection was deleted keeps its text and says so. Each folder keeps its own
tabs. **Settings → Editor → Reconnect the last connections at startup** connects the restored
tabs (it may ask for a password), and still runs nothing.
