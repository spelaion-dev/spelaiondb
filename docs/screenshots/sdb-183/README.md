# Search engines in the real desktop app

These are captures of the current real Tauri app on Windows, using a newly built debug
executable with bundled frontend assets. The test starts it offscreen with a temporary
workspace, personal folder and WebView2 profile, and removes that profile after exit.

All four search engines connect to their own isolated containers through the first-party
plugin, and the check runs one query on each. The picker is individually searchable and
Open Distro is absent. The HTTP editor's method/JSON highlighting and JSON formatting are
exercised with Elasticsearch's typed rows visible in the ordinary canvas grid.

| Viewport | Light | Dark |
| --- | --- | --- |
| 1280 × 800 | [Light](picker-light-1280.png) | [Dark](picker-dark-1280.png) |
| 900 × 600 (configured minimum) | [Light](picker-light-900.png) | [Dark](picker-dark-900.png) |

The original planned-only picker captures are preserved for comparison:
[light 1280](before-picker-light-1280.png), [dark 1280](before-picker-dark-1280.png),
[light 900](before-picker-light-900.png), [dark 900](before-picker-dark-900.png).

| HTTP console and results | Light | Dark |
| --- | --- | --- |
| 1280 × 800 | [Light](http-light-1280.png) | [Dark](http-dark-1280.png) |
| 900 × 600 | [Light](http-light-900.png) | [Dark](http-dark-900.png) |

The existing layout remains readable at both sizes, with horizontal scrolling for columns
at the minimum size. Theme changes fire the app's theme-change event so its canvas repaints
before the capture. No CSS colors or layout tokens change in this contribution.

Reproduce after building the app, from `apps/desktop`:

```powershell
$env:SPELAION_E2E_EXE = 'D:\path\to\target\debug\spelaion-desktop.exe'
$env:SPELAION_E2E_SEARCH_ENABLED = '1'
# Set SPELAION_DEV_PLUGIN_ELASTICSEARCH, OPENSEARCH, SOLR and MEILISEARCH to the search plugin.
node e2e/search-catalog.mjs
```

The plugin repository's `ci/compose.yaml` starts the four isolated servers at the test's
documented ports. Without `SPELAION_E2E_SEARCH_ENABLED`, the check verifies only the picker.
The focused check complements the full application
smoke and the workspace's database integration tests.
