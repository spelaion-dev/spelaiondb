# ADR-062: Desktop styling and layout

- **Status:** Accepted (implemented)
- **Date:** 2026-11-20
- **Supersedes:** the Tailwind, Radix (shadcn/ui) and `dockview` choices in
  [ADR-010](0010-frontend-and-results.md)

## Context

SDB-77 records the stack the desktop uses. The stylesheet defines the light and dark themes
with CSS custom properties. The app's React components implement its controls, menus, dialogs
and workspace panels. The UI guidelines describe their appearance and interaction.
[Desktop UI guidelines](../ui-guidelines.md)

## Decision

- **Use plain CSS with shared design tokens.** The light and dark theme blocks in `styles.css`
  define the visual roles described in the UI guidelines. Components use the shared classes and
  tokens, and the canvas grid reads the same custom properties. The build checks token use
  against the existing baseline and tests contrast in both themes.
  [Desktop stylesheet](../../apps/desktop/src/styles.css)
- **Keep the desktop controls in the app.** Prefer native controls and semantic HTML. The app
  owns keyboard behavior where it is needed. The shared modal focus hook keeps focus in the
  topmost visible dialog and restores it to the opener when possible. The menu placement
  helper keeps menus in the window.
  [Modal focus](../../apps/desktop/src/modalFocus.ts),
  [menu placement](../../apps/desktop/src/menuPlacement.ts)
- **Keep the panel layout in React and CSS.** The app lays out the activity bar, explorer and
  tabbed workspace, and the editor tab lays out the query editor and results. The app's
  separators support pointer and keyboard resizing.
  [App layout](../../apps/desktop/src/App.tsx),
  [editor tab](../../apps/desktop/src/components/EditorTab.tsx)

## Alternative

Adopt individual unstyled Radix primitives for dialogs, menus or popovers. They provide
keyboard and focus behavior and can use the existing visual tokens. Each primitive would
need to integrate with the app's behavior, including nested dialogs and focus restoration.
[Radix Primitives accessibility](https://www.radix-ui.com/primitives/docs/overview/accessibility)

## Revisit if

If screen-reader validation in SDB-64 finds recurring focus or menu defects in the app's
controls, trial the relevant Radix primitive and check the affected interaction in the real
desktop app with a screen reader.
