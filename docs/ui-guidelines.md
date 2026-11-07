# Desktop UI guidelines

These guidelines describe the direction for SpelaionDB's desktop app. They apply to new
work and to the parts of an existing screen that a change touches; they are not a claim that
every existing screen already meets them. Accepted ADRs and `AGENTS.md` take precedence.

## Product character

SpelaionDB is a database workbench, not a dashboard. Favor a calm, compact interface that
keeps names, SQL, data and the current connection easy to scan during long sessions. Use
space to separate tasks and establish hierarchy, not to add decorative cards or oversized
headings. Preserve room for the explorer, editor and results at the same time.

Start every change with the user's task and its consequences. The current connection,
database or schema, selection, connection state, pending changes and primary action must be
clear without relying on color alone. Keep frequently used actions close to the work they
affect, and avoid moving established controls solely for visual novelty. The editor's
compact vertical action column is an intentional part of the workspace.

## Visual system

- Reuse the theme variables and existing control patterns in `apps/desktop/src/styles.css`.
  Add semantic tokens when needed rather than scattering one-off colors and dimensions.
- Support both light and dark themes. Check text, icons, selected states and focus indicators
  against their actual backgrounds. Normal text should meet a 4.5:1 contrast ratio; do not
  assume white text works on every colored badge or button.
- Use the accent for selection, focus and primary actions; reserve danger styling for actions
  or messages that need it. A connection type's badge follows ADR-029: do not add per-connection
  color stripes or move safeguard decisions into the UI.
- Keep typography and spacing consistent with nearby views. Use monospace for SQL, values,
  paths and identifiers where it aids recognition; normal interface text remains proportional.
- Icons used without visible text need an accessible name and a discoverable explanation or
  shortcut. Do not rely on hover for essential status, errors or the only way to act.

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

## Forms, dialogs and feedback

- Group related fields under meaningful headings. Every field needs a visible label or, where
  a compact technical control makes that impractical, a clear programmatic name. Place help
  and validation errors next to the relevant control and make recovery possible.
- Keep the principal action and a safe dismissal path visible. Destructive confirmations must
  identify the target and consequences, show the SQL when the core requires it, and start with
  the safe action focused. Never make a security decision in React.
- Dialogs must manage initial focus, keep keyboard interaction in the active dialog, close only
  the topmost dialog on Escape where cancellation is allowed, and return focus sensibly. Do
  not discard entered text or pending edits without an appropriate warning.
- Treat loading, empty, populated, disabled, success and error states as part of the design.
  Explain why an action is unavailable when the reason is not obvious. Show actionable errors
  in the interface rather than only in a tooltip, color, console or transient notification.
- Make explicit whether an action runs SQL, opens SQL for review, changes a local preference,
  or writes to a shared project. Do not automatically reconnect or re-run user work.

## Layout, keyboard and language

- Test the normal desktop layout and the configured minimum window size, including open side
  panels and long names. Important controls must remain reachable; wrap or scroll deliberately
  rather than clipping them. Resizable separators need a keyboard equivalent and visible focus.
- Prefer native controls and semantic HTML. Interactive elements need accessible names,
  discernible focus, a logical tab order and keyboard behavior matching their role. Avoid
  placing interactive elements inside other interactive elements.
- Respect reduced motion. Do not use color as the sole indicator of status or risk.
- All UI strings go through the existing English and Spanish translations. Allow for longer
  translations and platform-specific rendering; do not put user-facing strings in CSS.

## Verification

For each UI change:

1. Run `npm run build` in `apps/desktop` and focused tests for changed behavior. Add regression
   coverage for new interaction behavior where practical.
2. Inspect the real Tauri app, not just source code, in light and dark themes and at a normal
   and constrained window size. Check relevant empty, loading, error, selected and disabled
   states with representative data. Use isolated test data and configuration.
3. Exercise keyboard focus, resizing, menus and dialogs, including nested dialogs where
   applicable. Check risky actions and unsaved changes without using a real production database.
4. Run `node e2e/smoke.mjs` after a Tauri build where supported. The current smoke test uses
   Windows/WebView2; report explicitly when it or a platform-specific check could not run.

Visual review and automated tests complement each other. A screenshot does not verify
accessibility, and a passing build or smoke test does not establish visual quality. Exercise
canvas-grid announcements and modal focus with an actual screen reader as well; semantic markup
alone does not demonstrate that the interaction is accessible.
