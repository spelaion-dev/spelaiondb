# ADR-084: Saved ER diagrams are local data: an arrangement over the catalog, read again when opened

- **Status:** Accepted
- **Date:** 2026-11-25
- **Builds on:** ADR-046 (ER diagrams laid out in the core), ADR-011 (local storage), ADR-022
  (what a repository may bring), ADR-040 (colors from the theme)

## Context
ADR-046 draws a read-only diagram laid out by the core and left saved diagrams for later,
saying they would be "local data or project files". People who document a schema want to keep
a diagram: move tables by hand, add notes, color groups of tables and open it again later.
Two places could keep one:

- **The local store** (`state.db`, ADR-011), next to the explorer's filters and bookmarks and
  the table data tab's filters: per workspace and connection, never shared.
- **Files in the project** (`.spelaion/diagrams/`), versioned and reviewed with the team's
  connections. A repository's files are used only once it is trusted (ADR-022), and a
  diagram's notes are text written by someone else.

## Decision
1. **Saved diagrams are local data in the store**, per workspace and connection, like the
   explorer's filters and bookmarks (`spelaion-store`'s `er_diagrams.rs`). They are never
   written to the repository, to a configuration file or to the MCP server's answers, and
   agents do not read them. A name is unique per workspace and connection.
2. **A saved diagram is an arrangement, never a copy of the catalog:** its name, its scope
   (a schema with the explorer's table filter, chosen tables, or one table and its
   neighbours: ADR-046's scopes), its display options, the position of each table it placed
   (by schema and name), its notes (text, position, width, color) and its groups (a name, a
   color and their tables). No column, type or key is kept: those are read from the catalog.
   It is kept as one JSON document with a version (`spelaion-core`'s `er/saved.rs`), written
   from and read into the core's own types, never stored as the UI sent it.
3. **The core checks a document when it is saved and every time it is read back**: names and
   note texts bounded and plain text (`check_plain_text`, the rule of a connection's
   description: lines and tabs, no hidden characters), at most twice `MAX_DIAGRAM_TABLES`
   placed tables (so gone ones fit beside the ones read), 200 notes and 100 groups, a table in
   one group at most, coordinates finite and within ±1,000,000 pixels, colors from the palette
   by name. A document that fails when read is listed as unreadable and
   opens nothing: it is never applied in part.
4. **Opening one reads the catalog again** with ADR-046's read, on the tab's own session:
   - a table the diagram placed that is still in its scope goes where it was;
   - a **new** table (in the scope, not placed) is laid out by the core's layout among the
     other new tables only, and that block goes below the placed tables; it is marked new
     until the diagram is saved;
   - a table that is **gone** (placed, but no longer read in the scope) is drawn as a dashed
     box with its name where it was and says so; it is kept when the diagram is saved, so a
     table that could not be read this time is not lost, until the user removes it.
   Relations are routed by ADR-046's routing between boxes wherever they are.
5. **Manual positions:** dragging a table (or a group's frame, which moves its tables, or a
   note) moves it in the page; on release the core lays the diagram out again with the new
   positions, routing its relations. While it moves, the page moves the box and the ends of its
   relations only. Panning and zooming still never ask for a layout. The first move fixes
   every table where the automatic layout put it; until then a diagram follows the automatic
   layout, and "Lay out again" goes back to it, keeping notes and groups.
6. **Notes and groups are laid out by the core too**, so the screen and the exports agree: a
   note's text wrapped to its width in the measured monospace character, a group's frame
   around its tables with its name above them. **Colors are a named palette** (gray, blue,
   green, amber, red, purple), kept by name and drawn with the `--er-color-*` tokens of both
   themes; a group always shows its name and a note its text, never color alone.
7. **Exports:** PNG and SVG draw the diagram on screen, notes, groups and positions
   included. Mermaid and DBML stay what ADR-046 says: the whole model, tables and keys, read
   from the catalog; notes, groups and positions are not written into them.
8. **Where it lives:** a diagram tab's toolbar has Save (a name is asked the first time),
   Add note and a menu with Save as…, Rename…, Lay out again and Delete saved diagram…; the
   command palette lists the saved diagrams of the open folder's connections and opens one in
   a tab of its own; a saved diagram's tab is called by its name and marks unsaved changes.

## Consequences
- A diagram is the user's own: a teammate does not see it, and it does not follow the user to
  another machine. Sharing one is a later decision (see "Revisit if").
- The schema-wide read of ADR-046 is unchanged: a saved diagram of a large schema reads one
  `describe_table` per table each time it opens.
- Positions are in the layout's pixels at the font the UI measured; a different interface
  font size moves nothing, but boxes that grow may overlap until the user moves them or lays
  the diagram out again.

## Alternatives
- **Files in the project (`.spelaion/diagrams/`) for team sharing:** reviewed in git, but a
  diagram of a personal connection has no repository, and a repository's diagram would be
  content from others: used only from a trusted repository, pinned like connections
  (ADR-022) or shown as text from someone else, and a merge conflict away from losing
  positions. The document of point 2 is self-contained, so files can come later without
  changing what is kept here.
- **Keeping the catalog in the saved diagram:** it would open without a connection, but it
  would show a schema that is no longer true, which is what point 4 avoids.
- **Positions in the UI's state only, laid out by the UI after a drag:** quicker to drag, but
  the routing would be written twice and the exports would not match the core's layout.

## Revisit if
Teams ask to share diagrams: then a diagram can be exported as a file and read from a trusted
repository's `.spelaion/diagrams/`, with this local copy as the user's own.
