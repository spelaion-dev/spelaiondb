// What the views ask about our plugins, answered in one place (SDB-373) from what the core and
// the adapter say (`PluginItem.availability`, `PluginItem.install`): whether an engine needs its
// plugin installed, what Install fetches, and what an install is doing. The Plugins page, the
// "add connection" picker and a connection that could not open all use these.
// `scripts/plugin-status.test.mjs` runs them.

import type { PluginItem, PluginsOverview } from "./bindings";

/** Whether no build of `plugin` is started here, so its engines cannot connect yet. */
export function needsInstalling(plugin: PluginItem): boolean {
  switch (plugin.availability) {
    case "inUse":
      return false;
    case "refused":
    case "missing":
    case "notInstalled":
      return true;
    default: {
      const unknown: never = plugin.availability;
      return unknown;
    }
  }
}

/**
 * The plugin of ours that serves `engine` when no build of it is started here: the engine
 * needs it installed. Null for an engine compiled into the app, or whose plugin runs.
 */
export function pluginNeeded(overview: PluginsOverview | null, engine: string): PluginItem | null {
  if (!overview) return null;
  return overview.plugins.find((plugin) => needsInstalling(plugin) && plugin.engines.includes(engine)) ?? null;
}

/** What clicking Install or Update fetches: the offered build, when it is new here or newer. */
export function installable(plugin: PluginItem) {
  return plugin.offered && (needsInstalling(plugin) || plugin.update !== null) ? plugin.offered : null;
}

/** What a plugin's install is doing, in words (the settings namespace's), wherever it is shown;
 *  null when idle. */
export function installText(
  plugin: PluginItem,
  t: (key: string, values?: Record<string, unknown>) => string,
  locale: string,
): string | null {
  const install = plugin.install;
  switch (install.kind) {
    case "idle":
      return null;
    case "downloading": {
      const total = install.total ?? 0;
      return total > 0
        ? t("plugins.downloading", { received: byteSize(install.received ?? 0, locale), total: byteSize(total, locale) })
        : t("plugins.installing");
    }
    case "failed":
      return t("plugins.installError", { detail: install.why });
    default: {
      const unknown: never = install;
      return unknown;
    }
  }
}

/** A size in bytes, for an archive to download. */
export function byteSize(bytes: number, locale: string): string {
  const units = ["B", "KB", "MB", "GB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  const digits = unit === 0 || value >= 10 ? 0 : 1;
  return `${new Intl.NumberFormat(locale, { maximumFractionDigits: digits, minimumFractionDigits: digits }).format(value)} ${units[unit]}`;
}
