# Smarter SQL completion — native UI verification

Before images use the exact frontend from `2eb19e4` (the parent of this change), packaged
with the unchanged Rust backend. After images use this branch's frontend and backend.
All images are genuine Windows 11 Tauri/WebView2 captures, with an isolated temporary
personal profile and the demo SQLite database. `SELECT 1` explicitly connected the tab;
the incomplete SQL shown in the editor was never executed.

| Theme and WebView2 viewport | Before | After |
|---|---|---|
| Light, 1400 × 900 | [Before](before-light-1400.png) | [After](after-light-1400.png) |
| Dark, 1400 × 900 | [Before](before-dark-1400.png) | [After](after-dark-1400.png) |
| Light, minimum 900 × 600 | [Before](before-light-900.png) | [After](after-light-900.png) |
| Dark, minimum 900 × 600 | [Before](before-dark-900.png) | [After](after-dark-900.png) |

The before editor offers no CTE columns after `r.`. The after editor offers the CTE's
three projected columns. The popup fits both viewports, preserves the editor/results
split, and uses the existing theme tokens. Selected labels and detail text were measured
against their composited backgrounds and meet 4.5:1 contrast in both themes.

`apps/desktop/e2e/completion.mjs` reproduces the after captures and checks alias columns,
CTEs, derived tables, WHERE context, foreign-key predicate insertion, arrow/Enter selection,
single-step undo, and the existing upper/lower/as-typed keyword preference. A CDP function-call
breakpoint observes Tauri command names, positively checks that `workspace_info` is seen, then
asserts zero catalog or execution calls while typing. The parser
regressions cover all six dialects, escaped identifiers, PostgreSQL case collisions,
recursive/union scopes, incomplete syntax, dialect comments, and bounded fallback behavior.
Cache tests cover database/workspace/source-session isolation, refresh generations, errors,
in-flight cancellation and release before connection completes. A [native error-state capture](metadata-error-dark-900.png)
uses a deliberately missing metadata-only test session; restoring the transport and refreshing
the explorer recovers completion. The editor session stays connected throughout.

Metadata uses the explorer's unfiltered catalog commands on a dedicated background session,
one table description at a time. Refreshing a connection/schema/table or running possible DDL
invalidates the editor cache. Metadata errors remain visible in the tab with refresh guidance.
The clone sees committed catalog metadata; tab-local temporary objects and uncommitted DDL
are outside this cache. The tab's active schema is read separately for unqualified names.
Rich completion is bounded to 300,000 characters, 64 parenthesis levels and 256 query scopes;
large files first try CodeMirror's current statement, then retain basic cached schema/keyword
completion if rich resolution exceeds those limits. None of these limits affects execution.

Local verification: frontend build and all 79 frontend regressions; workspace formatting,
Clippy with warnings denied, and 639 Rust tests with all seven Docker service endpoints
(two existing tests ignored); native debug build; focused completion checks; full native
smoke suite with real database, SSH, proxy and MCP paths. No dependencies changed.

Not checked here: macOS/WKWebView, Linux/WebKitGTK, an actual screen reader, optional
Kubernetes or external SSH-agent scenarios. Viewports were resized through WebView2 CDP;
these captures verify the client layout rather than native window decorations.
