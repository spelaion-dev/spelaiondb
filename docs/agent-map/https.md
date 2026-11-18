# Https

- `crates/spelaion-https`: the HTTPS transport the app gives `spelaion-core::plugins::fetch`
  (ADR-055): the updater's `reqwest`, HTTPS only (a redirect too), the system's proxies, no
  cookies or credentials. It only moves bytes: what is fetched and kept is the core's. Tested
  against local servers; `-- --ignored` also fetches a pinned archive from GitHub. The Plugins
  page's commands are `apps/desktop/src-tauri/src/plugins.rs`: they take a name, a version or a
  picked file, never an address (`scripts/plugin-commands.test.mjs`), and swap `AppState.engines`
  after each change, so the next connection starts the new build without a restart. The page
  itself is `src/components/PluginsSettings.tsx` (Settings → Plugins) and only shows the core's
  overview; `src/plugins.ts` shares it with the "add connection" picker and a connection that
  could not open (`MissingPlugin.tsx`), which send to the page when an engine's plugin is not
  installed.
