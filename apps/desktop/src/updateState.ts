// What the update's state means for the page (SDB-377), kept free of React and of the app's
// API so `scripts/update-state.test.mjs` can check it.

import type { InstallStatus_Serialize as InstallStatus, UpdateMethod, UpdateStatus_Serialize as UpdateStatus } from "./bindings";

/** The text that says how an update the app does not install is installed, or `null` when the
 * app installs it itself. Keys exist in both the `app` and the `settings` namespaces. */
export function manualUpdateKey(method: UpdateMethod): "manualDeb" | "manualRpm" | "manualDownload" | null {
  switch (method) {
    case "inApp": return null;
    case "deb": return "manualDeb";
    case "rpm": return "manualRpm";
    case "releasesPage": return "manualDownload";
  }
}

/** Where installing the update stands, for one the app installs itself; `null` otherwise. */
export function installOf(status: UpdateStatus): InstallStatus | null {
  switch (status.kind) {
    case "unknown":
    case "unavailable":
    case "upToDate":
      return null;
    case "available":
      return status.method === "inApp" ? status.install : null;
  }
}

/** The download is running: the install buttons wait. */
export function isInstalling(status: UpdateStatus): boolean {
  return installOf(status)?.kind === "downloading";
}

/** What a download shows: its share when the size is known, else what arrived (in MB). Keys
 * exist in both the `app` and the `settings` namespaces, under their update group. */
export function downloadText(install: InstallStatus, language: string): { key: "downloading" | "downloadingSize"; values: Record<string, string> } | null {
  switch (install.kind) {
    case "ready":
    case "failed":
      return null;
    case "downloading": {
      const total = install.total ?? null;
      if (total !== null && total > 0) {
        const percent = Math.min(100, Math.floor(((install.received ?? 0) / total) * 100));
        return { key: "downloading", values: { percent: new Intl.NumberFormat(language).format(percent) } };
      }
      const size = new Intl.NumberFormat(language, { style: "unit", unit: "megabyte", maximumFractionDigits: 1 });
      return { key: "downloadingSize", values: { received: size.format((install.received ?? 0) / 1_000_000) } };
    }
  }
}

/** A request of the page's own: one at a time, and what it ended with when it failed. */
export type UpdateRequest =
  | { kind: "idle" }
  | { kind: "checking" }
  | { kind: "failed"; action: "check" | "install" | "openReleasesPage" | "setCheckAtStartup"; message: string };

/** What went wrong, for the place that shows it: the core's reason when an install failed,
 * else the page's own request that failed. */
export function updateProblem(state: { status: UpdateStatus; request: UpdateRequest }): { action: "check" | "install" | "openReleasesPage" | "setCheckAtStartup"; message: string } | null {
  const install = installOf(state.status);
  if (install?.kind === "failed") return { action: "install", message: install.reason };
  switch (state.request.kind) {
    case "idle":
    case "checking":
      return null;
    case "failed":
      return { action: state.request.action, message: state.request.message };
  }
}

