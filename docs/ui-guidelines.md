# Desktop UI guidelines

These guidelines set how SpelaionDB's desktop app looks and behaves: its look, where actions
and shortcuts go, and how dialogs, errors and long operations read. Read them before any UI
change. They apply to new work and to every part of a screen that a change touches; they do not
claim that every screen already meets them (the known gaps are Plane Backlog items). Accepted
ADRs and `AGENTS.md` take precedence.
The values live in `apps/desktop/src/styles.css`, in its `:root` and `:root[data-theme="dark"]`
blocks; this document names their roles and says how to use them.

## Direction

SpelaionDB is a database workbench, not a dashboard: people spend hours in it reading names,
SQL and data. The goal is an interface that is calm, clear, compact and current, and each of
those words is a rule:

- **Calm.** Neutral surfaces and one accent. Color only carries meaning: selection, focus, the
  primary action, status and a connection's type (ADR-029). No decorative gradients, shadows,
  cards or tinted panels.
- **Clear.** Hierarchy comes from size, weight and space, in that order, not from boxes, lines
  or color. Each surface has one obvious primary action. State is written as text, never shown
  by color alone.
- **Compact.** The explorer, the editor and the results fit together at 1280×800. Density
  comes from consistent heights and spacing, not from text smaller than 11px.
- **Quiet until needed.** Secondary actions appear on hover, focus or selection and are always
  reachable from the keyboard and the context menu. Advanced options start collapsed. Say
  things once: a connection picker that reads `Payments (sqlite, Payments)` says it twice.
- **Current.** The system font, one icon set, 6px corners, thin themed scrollbars, short
  motion, and a dark theme designed on its own terms rather than inverted.

Start every change from the user's task and its consequences. The current connection, database
or schema, selection, connection state, pending changes and primary action must be clear at a
glance. Keep frequent actions next to the work they affect and do not move established
controls for novelty: the editor's compact vertical action column stays.

## Tokens

Use tokens, never literals. Outside the two token blocks, neither `styles.css` nor the canvas
code (`ResultGrid.tsx` reads the same variables) contains a color, font size, radius, shadow or
z-index literal. The exceptions are the engine logos' brand colors and the SQL syntax theme,
each in its own commented block. A value that is missing becomes a token named by its role,
defined for both themes in the same change.

### Spacing

A 4px grid: `--space-2xs` 2px, `--space-xs` 4px, `--space-sm` 6px, `--space-md` 8px,
`--space-lg` 12px, `--space-xl` 16px, `--space-2xl` 24px, `--space-3xl` 32px. The 2px and 6px
steps are for the inside of dense controls (an icon and its label, a badge's padding). Related
things sit closer than unrelated ones: 4–8px inside a group, 12–16px between groups, 24px
between the sections of a page or dialog.

### Type

`--font-ui` is the system font (Segoe UI Variable, SF Pro, the desktop's font on Linux), with
no web fonts; `--font-code` is the monospace stack. Each size has a matching line height,
`--leading-xs` to `--leading-2xl`.

| Token | Size / line height | Used for |
|---|---|---|
| `--text-xs` | 11 / 16px | badges, captions, section labels, a grid column's type |
| `--text-sm` | 12 / 16px | secondary text, field labels, hints, metadata, the status bar |
| `--text-md` | 13 / 18px | the default: controls, menus, explorer, tabs, grid cells, SQL |
| `--text-lg` | 15 / 20px | dialog and panel titles |
| `--text-xl` | 18 / 24px | page titles, such as Settings |
| `--text-2xl` | 24 / 32px | the welcome screen's title only |

- Two weights: 400 and 600. No 500, 650, 700 or `bold`.
- Sentence case everywhere: titles, buttons, menu items, tabs and labels. Uppercase only for
  `--text-xs` section labels (600, `letter-spacing: 0.04em`); ADR-029's `PRODUCTION` badge in
  the editor's toolbar is the one other exception.
- Monospace for SQL, cell values, fingerprints and identifiers shown as code (type ids, paths
  in technical fields); proportional for names in the explorer, labels and prose.
- `font-variant-numeric: tabular-nums` for counts, durations, row numbers and sizes.
- Prose (dialog descriptions, help text) uses a 1.5 line height and lines of at most about 72
  characters.

### Color

Each role is defined for both themes:

| Role | Tokens | Rule |
|---|---|---|
| Surfaces | `--bg`, `--panel`, `--sidebar`, `--activity-bg` | flat; separate regions with a hairline or a change of surface, never both |
| Text | `--fg`, `--muted`, `--faint` | `--fg` and `--muted` reach 4.5:1 on every surface they appear on; `--faint` is for placeholders and disabled text, never for information |
| Lines | `--line`, `--line-strong` | `--line` for dividers; `--line-strong` for the borders of inputs, selects and secondary buttons, so a control is visible without hovering it |
| Interaction | `--hover`, `--accent`, `--accent-soft`, `--accent-text` | hover is a neutral tint; a selected row or item is `--accent-soft` with `--fg` text; only the primary button is filled with the accent |
| Status | `--success`, `--warning`, `--danger`, `--info`, each with `-soft` and `-fg` | always beside text or an icon; `-fg` text reaches 4.5:1 on `-soft` |
| Connection types | `--tone-{neutral,green,blue,amber,red}`, each with `-soft` and `-fg` | ADR-029's fixed palette: no free colors, no stripes |
| Chart series | `--chart-1` … `--chart-8`, `--chart-other` | ADR-040's categorical palette, in fixed order (series 1 is always `--chart-1`); only for chart marks, never text, and never color alone: a legend, labels or a tooltip name every series |

- One accent, blue. Red is for destructive actions, errors and Production; amber for pending
  or risky states; green for connected and completed. Never use a status color as decoration.
- In the dark theme raised surfaces are lighter than the base, text is off-white rather than
  pure white, and the accent and tones are lighter variants, each rechecked for contrast.

### Shape, depth and size

- Radius: `--radius-sm` 4px (badges, chips, checkboxes, the grid's selection), `--radius-md`
  6px (buttons, inputs, tabs, list and menu items) and `--radius-lg` 10px (menus, popovers,
  dialogs, toasts); dots are round.
- The workspace is flat. Only layers above it cast a shadow: `--shadow-popover` for menus,
  popovers, tooltips and autocompletion, and `--shadow-dialog` for dialogs, with `--backdrop`
  behind modal ones. Panels, cards and buttons have no shadow.
- Heights: `--control-sm` 24px (icon buttons in toolbars, inline controls), `--control-md`
  28px (buttons, inputs and selects) and `--control-lg` 32px (welcome and empty-state
  actions). Rows in lists, trees, menus and the grid are `--row-h` (26px); bars (the tab strip
  and the editor's and the results' toolbars) are `--bar-h` (36px).
- Stacking order, lowest first: `--z-sticky`, `--z-dropdown`, `--z-dialog`, `--z-popover` (a
  menu opened from a dialog shows above it) and `--z-toast`.

### Icons

- One outline set for the whole app: Lucide (`lucide-react`, ISC license) at 16px with its
  default stroke, 20px in the activity bar, in `currentColor`, centered in a square target of
  at least 24px. An icon Lucide lacks is drawn on the same 24px grid with the same stroke.
- No text characters as icons (`+`, `×`, `⋯`, `▸`, `▾`): their size and weight change with
  the font and the system.
- Engine logos keep their brand colors; nothing else in the interface is multicolored.
- An icon without visible text has an accessible name and a tooltip with its name and
  shortcut.

### Motion

`--duration-fast` (120ms) for hover, press and color changes; `--duration-overlay` (180ms) for
an overlay appearing (opacity, and a move of at most 4px); always with `--ease-out`. Never
animate data, the grid, layout changes or anything while the user types. Under
`prefers-reduced-motion` only opacity changes remain. A spinner appears only after 300ms.

## Components

- **Buttons.** *Primary* (filled with the accent; at most one per dialog, page or toolbar
  region), *secondary* (the default: panel with a `--line-strong` border), *ghost* (no border:
  toolbars, icon buttons, inline actions) and *danger* (filled red; only the button that
  confirms a destructive action). A label is a verb that names the result ("Delete
  connection", "Run 3 statements"), never "OK" or "Yes", and ends with "…" (one character)
  when it opens a dialog that asks for more.
- **Toolbars.** Ghost icon buttons grouped by task, groups separated by 12px or a single
  divider, never boxed. Text buttons only for the main action or where an icon would be
  ambiguous.
- **Tabs.** One active indicator: the `--panel` surface and a 2px accent edge. The close button
  shows on the active tab and on hover or focus; `Ctrl+W` always closes the active tab.
- **Explorer, lists and trees.** 26px rows, a 16px icon and the name in `--text-md`. The name
  comes first: badges and metadata sit on the right, shrink first and collapse to a dot with a
  tooltip when the sidebar is narrow. Row actions show on hover, focus and selection, and are
  always in the context menu.
- **Badges.** Soft: the tone's `-soft` background and `-fg` text, `--text-xs` at 600,
  `--radius-sm`, 2px by 6px of padding, the type's name in sentence case. Production's badge
  is the only solid one. One type badge per row at most; other flags (SSH, read-only) are
  neutral chips or icons with a tooltip.
- **Forms.** Labels above their fields in `--text-sm`; help under the field in `--muted`; an
  error under the field in red with an icon, saying what is wrong and how to fix it, keeping
  what was typed. A compact technical control without a visible label has an accessible name.
  Group fields with a heading and space rather than boxes: a `<fieldset>` keeps its meaning for
  assistive technology but draws no border.
- **Dialogs.** 400, 560 or 760px wide, 20–24px of padding, a `--text-lg` title and at most one
  sentence of context before the content; the actions at the bottom right, Cancel before the
  primary action. Lists inside a dialog are rows divided by hairlines, not a card per item.
- **Menus.** `--radius-lg`, `--shadow-popover`, 4px of padding, 26px items with a
  `--radius-md` hover, shortcuts right-aligned in `--muted`, 1px separators; destructive items
  come last, in red.
- **Empty states.** Centered in their region: an optional 24px icon in `--muted`, a one-line
  title, one sentence and at most one primary action. No illustrations.
- **Scrollbars.** Thin and themed in every scrolling region, the grid included: a transparent
  track and a thumb from a token that widens on hover. Check them on WebKitGTK.

## Workspace and data

- Keep the workspace identity readable separately from explorer tools. Long names and paths
  may truncate in compact regions, but provide a way to inspect the full value.
- In results, separate row operations from saving or discarding pending changes. Make pending
  changes visible and say when edits are not yet written. Preserve the core's confirmation,
  transaction and read-only rules.
- Keep a tab's transaction mode and its driver-reported transaction state visible, and make
  Commit, Rollback and the consequences of closing an open transaction clear (ADR-031). Never
  infer transaction state or decide its safeguards in the UI.
- State a filter's scope where it matters: the current table page, loaded query rows, or the
  full result. Do not imply a local result filter searches data that has not been loaded.
  Keep filtering, copying and exporting consistent with the core's view of the result.
- A cell panel must distinguish editable values from read-only values and `NULL` from an empty
  string. Make the selected column and row clear, and expose row and cell actions to keyboard
  users as well as pointer users.
- The result grid remains backed by the core and rendered as a canvas (ADR-010). Keyboard
  support alone does not make its contents accessible to screen readers: provide an accessible
  way to reach headers, values and selection when working on that area, without moving result
  data into React state.

## Where actions live

Each action has one home, named the same wherever it appears (menu item, tooltip, button).
Put a new action in the first place below that fits; a toolbar or hover button is a shortcut
to an action that is also in a context menu, never its only way in.

| Place | What goes there | Today |
|---|---|---|
| Editor action column | running and saving the tab's SQL | Run (Cancel while running), Run script, Run options, Format, Save, Save as |
| Tab bar (connection row) | what the tab is connected to and its state | connection and database (or schema) pickers, status with Reconnect, type badge, transaction controls; Refresh / Cancel on table tabs |
| Results toolbar and grid | the active result and its selection | result tabs, filter, Export…, chart (grid or chart of the result; Chart selected columns in the cell menu; the chart's own controls and Save image… above it), maximize; row edits with Save changes / Discard above the grid; copy, sort and filters in the grid's menus; in a table data tab, a foreign key cell's link and Open referenced row, and the side panel's references of the active row; the side panel's selection numbers (also in the cell menu); the cell panel's Save to file… and, in an editable result, Load from file…; in a table data tab, Column statistics in the header menu |
| Tab strip | tabs and files | new tab, the file menu (Open…, recent files: the app has no menu bar), each tab's context menu |
| Explorer header | the explorer itself | link with editor, connected only, quick filter (finds connections by name, tag, type or host, forgiving a typo), add connection |
| Explorer row menu | everything about that object or connection | open, generate SQL, copy names, filters, create, Import data… (a table, or a new table from a schema),  Move to folder… (also by dragging the connection onto a folder title), Rename…, Drop… (last, red); a connection's: New query, Open object by name…, Refresh all schemas, Edit…, Configure filters…, New schema, Duplicate, Copy, Paste connection, Move to folder…, Delete… (last, red) |
| Activity bar | panels and pages | explorer, processes; history, saved queries, MCP and Settings at the bottom |
| Settings | preferences for every workspace | applied and saved as they change, with no Save button |
| Command palette | every app command, and the actions of connections and loaded objects | Ctrl+Shift+P: commands with their shortcuts, connections, the explorer's loaded objects (open data, open structure, new console, copy names, show in explorer) and recent files |

- App commands (the tab menu, the file menu, the activity bar, the welcome screen and the global
  shortcuts) are registered once in `commandRegistry.ts`, with their name and shortcut, and what
  they do is in `commands.ts`. A menu item of those menus is a `CommandMenuItem`, never a button
  written by hand, so the palette lists it too; `scripts/commands.test.mjs` compares the registry
  with the menus, the locales and this shortcut map.
- The palette lists objects only from what the explorer has already read (`catalog.ts`): it
  never asks a database for more. Menus that act on a selection made in their own region (an
  explorer row, the grid's cells) stay in that region.

- An option of one connection belongs in its form (Edit connection…), one of one tab in that tab
  (Run options), one of the whole app in Settings.
- A connection row has no action buttons of its own: its context menu (right click, the Menu
  key or Shift+F10) holds every action on it, each once, and double-click opens a new query.
  Its personal options, such as saving its query history, are in its form.
- A row shows at most two action buttons of its own; the rest is in its menu.
- An action's label says whether it runs SQL ("Drop…", which shows the SQL first), opens SQL
  for review ("Generate SELECT", "Open in editor"), changes a personal preference or writes to
  the shared project.

### Shortcuts

- A shortcut goes to a frequent action, following DBeaver in the explorer and grid (F2, F5,
  Delete, Ctrl+Shift+D) and VS Code in the editor and tabs. A destructive shortcut opens the
  same confirmation as its menu item (Delete opens the Drop dialog).
- Global shortcuts are the registry's (`commandRegistry.ts`, scope `global`), run by `App.tsx`'s
  one handler (and the Ctrl+Tab switcher in `TabBar.tsx`). They do nothing while a modal is
  open, and never take a key that a focused area uses.
- Focused areas (editor, grid, explorer, tab strip) handle their own keys and may give a key a
  local meaning: Ctrl+S in the grid saves pending edits; Ctrl+Shift+C copies with headers in the
  grid and qualified names in the explorer. Text inputs keep their own keys (Ctrl+Z, Ctrl+A).
- Handlers accept Ctrl or Cmd (`ctrlKey || metaKey`). Ctrl+Alt is AltGr on Windows: match
  `e.key`, so a key that AltGr turns into a character never fires the shortcut.
- Written with modifiers in the order Ctrl, Shift, Alt and keys as printed on the keyboard
  (`Ctrl+Shift+Enter`, `Shift+Alt+F`, `Delete`). Buttons show it in the tooltip, `Label
  (Shortcut)`; menus right-align it in `.menu-shortcut`. The text comes from the command's entry
  in `commandRegistry.ts` (or, for a focused area's own key, one constant next to its handler),
  not from a translated string.
- A new shortcut is added to this map in the same change.

| Where | Keys |
|---|---|
| Anywhere | Ctrl+Shift+P command palette · Ctrl+T new query · Ctrl+Shift+T reopen closed tab · Ctrl+W close tab (or Settings) · Ctrl+Shift+W close all · Ctrl+Alt+W close others · Ctrl+Alt+R close to the right · Ctrl+F2 rename tab · Ctrl+Alt+D duplicate tab · Ctrl+Alt+P pin tab · Ctrl+Tab / Ctrl+Shift+Tab recent tabs · Ctrl+PageDown / Ctrl+PageUp next / previous tab · Ctrl+Shift+PageDown / Ctrl+Shift+PageUp move the tab · Ctrl+O open SQL file · Ctrl+S save the file tab, or the console's query by name · Ctrl+Shift+S save as file · Ctrl+Alt+M maximize results · Ctrl+Shift+D open object by name · Ctrl+Shift+F explorer filter · Ctrl+Alt+V import data from the clipboard into the active tab's connection · Ctrl+, Settings |
| SQL editor | Ctrl+Enter run selection or statement · Ctrl+Shift+Enter run script · Shift+Alt+F format · Ctrl+click / Alt+click add a cursor · Ctrl+/ line comment · Shift+Alt+A block comment · Alt+Z word wrap · Ctrl+Shift+[ fold selection · Ctrl+Shift+] unfold selection · Ctrl+Shift+\ matching bracket · Ctrl+Shift+Alt+U upper case · Ctrl+Shift+Alt+L lower case · Ctrl+Alt+Q lines to quoted list · Ctrl+Alt+S select statement · Ctrl+Alt+C copy statement · Ctrl+Shift+L select all occurrences · Ctrl+Alt+1 SELECT snippet · Ctrl+Alt+2 INSERT snippet · Ctrl+Alt+3 UPDATE snippet · Tab expand sel / ins / upd or next snippet field · Shift+Tab previous snippet field · Menu key / Shift+F10 editor actions · CodeMirror's defaults otherwise |
| Result grid | arrows, Home, End, PageUp, PageDown (Ctrl: to the edge; Shift: extend) · Ctrl+A all · Shift+Space rows · Ctrl+Space columns · Ctrl+C copy · Ctrl+Shift+C copy with headers · Enter cell viewer, Escape closes it · Ctrl+Enter or Ctrl+click open the row a foreign key cell references (table data tabs) · Ctrl+F find, F3 / Shift+F3 next / previous, Alt+C match case · Ctrl+Shift+J jump to column |
| Table data edits | F2 edit cell · Alt+Insert add row · Ctrl+Alt+Insert duplicate row · Delete or Alt+Delete delete rows · Ctrl+Z undo the selected rows' edits · Ctrl+Alt+N set NULL · Ctrl+S save changes · Ctrl+R discard all |
| Explorer row | Enter / Space expand · Ctrl+Enter or double-click open the overview (a connection: a new query) · Menu key / Shift+F10 a connection's menu · F5 refresh · F2 rename · Delete Drop… · Ctrl+C copy names (a connection row copies the connection) · Ctrl+Shift+C qualified names · Ctrl+V paste a connection |
| Tab strip | arrows, Home, End move between tabs · F2 rename · Menu key or Shift+F10 tab menu |
| Command palette | Up / Down, PageUp / PageDown move · Enter runs a command, or lists an object's actions · Escape or Backspace (empty box) goes back from an object's actions, Escape closes |
| Dialogs, menus, separators | Escape cancels the topmost · Enter submits a one-field dialog · Tab stays inside a modal · arrows resize a separator (Shift: bigger steps), Home / End to its limits |

## Dialogs and confirmations

- **A modal dialog** asks something the work cannot go on without (a password, a server
  fingerprint, query parameters, unsaved changes, an open transaction), confirms what loses
  data or reaches others, or holds a short form (rename, filters, a new element). Large forms
  (a connection, Settings) are wizard-sized modals.
- **Inline feedback** reports the result of an action where it was taken (see the next section).
  There are no toasts, and no dialog only to say something worked.
- **A banner** (`.banner`, `role="status"` or `alert`) holds a state of a whole region until it
  is resolved or dismissed: a failed load with Retry, restored-tab notes, an update, the MCP
  production warning.
- **A system notification** only tells that a long run finished while the app was in the
  background (opt-in, ADR-035), never with SQL, data or error text. Native dialogs only choose
  files and folders (`dialogs.ts`).

A confirmation reads:

- **Title:** a question that names the action, and the target when it is short: "Delete
  connection?", "Reload orders.sql from disk?", "Close with an open transaction?".
- **Body:** what will happen and to what: the connection by name, the file, who else is
  affected ("your team will lose it too when you commit the change"), what is lost and what is
  not ("Nothing has run yet."). Show the SQL the core will run, when it gives one. A muted line
  may say how to avoid the loss ("To keep the changes, cancel and press Commit first").
- **Buttons:** Cancel first, the action last, named by what it does: "Close and roll back",
  "Clear the log", "Don't save" / "Save", "Run anyway". Never OK, Yes, No or Confirm. Only the
  button that loses data or cannot be undone is danger; when a run is confirmed only because
  the connection's type asks (ADR-029), its button is secondary ("Run").
- **Keyboard:** the safe choice has the focus in a destructive confirmation, so Enter by habit
  never drops or deletes; a form focuses its first field; a non-destructive choice focuses its
  primary answer. Escape is the safe choice (Cancel; Stop for a paused script).
- **Always a dialog**, never a button that changes to "Click again to delete". Closing or
  leaving never drops typed text, unsaved SQL or pending edits without one.
- **While it runs**, its buttons are disabled, the action reads "Running…" or "Deleting…" and
  Escape does nothing. If it fails, the dialog stays open with the error inside it
  (`role="alert"`) and what was typed is kept.
- Dialogs opened over a form (trust, password, fingerprint) take Escape first; only the topmost
  closes, and focus returns to what opened it (`modalFocus.ts`). Never decide a safeguard in
  React: dialogs show what the core decided (`check_script`, ADR-008, ADR-029).

## Errors, empty and disabled states

Loading, empty, populated, disabled, success and error are all states to design.

- **Wording:** "Could not *action*: *detail*" (`Could not connect: …`, `Could not save
  orders.sql`), the detail as the core gave it, never reworded or dropped. Then, when it is not
  obvious, what to do: "Reconnect, then run it again: nothing is re-run on its own."
- **Next to the cause:** under the field for validation; inside the dialog, above its buttons,
  for a dialog's action; in the results' Messages (with Detail, Hint and Code) and one line in
  the status line for a query; under the row (`row-note`) in the explorer; in the tab bar
  (`notice`) for an action of the tab; in a banner with Retry when a region could not load. A
  lost connection shows "Connection lost" in the tab with Reconnect.
- Never only in a tooltip, a color or the console. An error goes away when the user changes what
  caused it or retries.
- **Disabled actions** say why when it is not obvious, in their tooltip or a note: "Apply or
  discard the pending changes first", "The connection is read-only: the SQL can only be opened
  in the editor."
- **Empty states** (`.empty`, centered, muted) say what is missing and how to get it: "Write a
  query and press Ctrl+Enter.", "No saved queries yet. Ctrl+S in an editor tab saves its SQL."
  Nothing yet and nothing matching are different texts ("No matching queries."), and loading is
  its own state ("Loading history…"), so an empty state never shows while data is coming.

## Long operations

Query runs, table pages, query counts, exports, connection tests, explorer loads and kubectl
lookups follow the same rules:

- **Start:** a status text at once ("Running…", "Refreshing…", "Loading databases…"); a spinner,
  if any, only after 300ms.
- **Progress:** what is really known, never an invented percentage: each statement's message,
  rows written and size, table 2 of 5, each step of a connection test with its time.
- **Cancel:** what reaches a server or writes a file can be cancelled from where it started:
  Run becomes Cancel in the same place, an export dialog has Cancel export. The button is
  secondary, named "Cancel" plus the object where it stands alone, and asks the core
  (`cancel`, `cancelExport`); the UI shows what the core reports ("Query cancelled", "Export
  cancelled: no file was written."), not what it assumes.
- **Meanwhile:** the rest of the app stays usable: other tabs, the explorer, and the editor's
  text. On the busy tab, only what would conflict is disabled (Run script, Run options, the
  database picker, paging, edits); a tab that is running shows a spinner in the strip, and
  closing it asks first ("Close and cancel").
- **End:** the result appears where it started (the status line's rows, summary and duration).
  Nothing is re-run on its own after a reconnect.

## Layout, keyboard and language

- Test the normal desktop layout and the configured minimum window size, including open side
  panels and long names. Important controls must remain reachable; wrap or scroll deliberately
  rather than clipping them. Resizable separators need a keyboard equivalent and visible focus.
- Prefer native controls and semantic HTML. Interactive elements need accessible names, a
  logical tab order and keyboard behavior matching their role. Avoid placing interactive
  elements inside other interactive elements.
- Keyboard focus is a 2px `--accent` outline with a 2px offset (inset in dense lists and the tab
  strip) on every focusable element. Never rely on hover alone for status, errors or the only
  way to act.
- Respect reduced motion. Do not use color as the sole indicator of status or risk.
- All UI strings go through the existing English and Spanish translations. Allow for longer
  translations and platform-specific rendering; do not put user-facing strings in CSS.

## App icon

The icon is the first thing anyone sees of the brand, at 16px in a taskbar as often as at
256px in an installer. The current one, a database cylinder lit by a caver's lamp and drawn
with gradients, says "database" generically and loses its detail below 48px. Its replacement:

- has one bold silhouette that is recognizable at 16px and in a single color (system trays,
  monochrome taskbars);
- uses at most two brand colors and a neutral, close to the interface's accent, in flat shapes
  or one subtle gradient: no gloss and no thin lines that vanish when small;
- carries the idea of Spelaion (from σπήλαιον, a cave: exploring, light in the dark, layers)
  without the stock database cylinder;
- works as a family: SpelaionSSH (ADR-023) shares the mark and changes one element or color;
- has a single SVG source, `apps/desktop/src-tauri/icons/icon.svg`, from which
  `npx tauri icon` generates every size, with hand-tuned 16px and 32px versions if the
  generated ones blur, and is checked on light and dark taskbars on Windows, macOS and Linux.

## Verification

For each UI change:

1. Run `npm run build` in `apps/desktop` and focused tests for changed behavior. Add regression
   coverage for new interaction behavior where practical.
2. Inspect the real Tauri app, not just source code, in light and dark themes and at a normal
   and constrained window size. Check relevant empty, loading, error, selected and disabled
   states with representative data. Use isolated test data and configuration. Put
   before-and-after screenshots of both themes in the pull request.
3. Check every color you add or change against its real backgrounds in both themes: 4.5:1 for
   text; 3:1 for icons, focus outlines and any border that alone shows a control's shape or
   state, such as a checkbox's.
4. Exercise keyboard focus, resizing, menus and dialogs, including nested dialogs where
   applicable. Check risky actions and unsaved changes without using a real production database.
5. Run `node e2e/smoke.mjs` after a Tauri build where supported. The current smoke test uses
   Windows/WebView2; report explicitly when it or a platform-specific check could not run.

Visual review and automated tests complement each other. A screenshot does not verify
accessibility, and a passing build or smoke test does not establish visual quality. Exercise
canvas-grid announcements and modal focus with an actual screen reader as well; semantic markup
alone does not demonstrate that the interaction is accessible.

Before committing, check that:

- no color, font size, radius, shadow or z-index is written outside the token blocks;
- each surface has at most one primary button, and no text character stands in for an icon;
- every action shown only on hover is also reachable from the keyboard and a context menu;
- a new action sits where "Where actions live" puts it, and a new shortcut is in the map and
  takes no key a focused area already uses;
- confirmations, errors and long operations read and behave as their sections say;
- every new string exists in English and Spanish;
- both themes were inspected, and anything that could not be checked is reported.
