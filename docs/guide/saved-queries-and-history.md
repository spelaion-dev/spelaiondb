# Saved queries and query history

## Saved queries

**Ctrl+S** (or **Save** in the editor toolbar) saves the tab's SQL under a name, together with
its connection; saving again offers the same name, and an existing name is replaced.
**Saved queries** in the sidebar lists them with a search box: **Open** puts one in a new tab
(without connecting or running it) and **Delete** asks for a second click. They live in the
local store on this computer, per repository, or for every repository when saved outside one.

## Query history

Open **Query history** in the sidebar to search executed SQL or connection IDs and open a
statement in a new, disconnected editor tab. Opening it never runs it. History records each
executed statement with its connection, start time, duration, returned row count and error
(including cancellations), and survives app restarts.

**Save query history**, in the "⋯" menu next to each connection in the sidebar, controls
recording for that connection. Disable it
before running SQL containing passwords or other secrets: SQL text and database errors are
stored as written. The preference is personal, takes effect across tabs and survives app
restarts. Disabling it keeps existing entries; it does not erase previously saved SQL.

History and preferences live in `state.db` in the personal configuration directory, outside
the repository. Project connections are separated by project; personal connections share
their history across folders. This setting controls desktop history; the MCP audit log is
separate.
