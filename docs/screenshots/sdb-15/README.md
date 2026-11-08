# Editor ergonomics screenshots

Captured from the real Tauri app on Windows with WebView2, using isolated temporary profiles.
The SQL shown is editor text and was not executed.

- `before-*`: the unmodified frontend from `dev` at `29b196a`, built from its original sources
  and embedded in the native app. The backend includes the new editor-preference commands,
  which this baseline frontend does not use.
- `after-*`: this feature's native build, with matching text, dimensions and themes.
- `after-palette-*`: the editor action running through the central command palette after
  SDB-18 was merged into `dev`, using the smoke test's isolated demo profile.
- `after-menu-minimum.png`: the real WebView2 viewport constrained to 900 × 600. The menu
  stays in view and supports Home, End, arrows and Escape. Editor settings were also checked
  at that size in both themes: Tab reaches every control and scrolls the last one into view.

The normal captures use a 1400 × 900 viewport. Light and dark captures cover the editor,
its action menu and Settings → Editor.
