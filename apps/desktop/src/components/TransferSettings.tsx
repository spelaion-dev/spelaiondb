// Settings › Export and import (SDB-31): the personal settings in one file, to move them to
// another computer. The core writes, reads and checks the file (`settings_transfer.rs`); the
// page chooses the file, shows what an import would change before it writes anything, and
// reloads its own preferences afterwards. The file holds no connection and no password.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { SettingsSummaryItem } from "../bindings";
import { chooseOpenFile, chooseSavePath } from "../dialogs";
import { usePreferences } from "../preferences";
import { useShortcuts } from "../shortcutPreferences";
import { useSnippets } from "../snippetStore";

const EXTENSION = "spelaion-settings";

/** Where the page is: only the page has this state, so it is a union ending in a `never`
 *  check (ADR-060). */
type Transfer =
  | { kind: "idle" }
  | { kind: "working"; action: "export" | "preview" | "import" }
  | { kind: "exported"; path: string; summary: SettingsSummaryItem }
  | { kind: "preview"; path: string; summary: SettingsSummaryItem }
  | { kind: "imported"; summary: SettingsSummaryItem }
  | { kind: "failed"; action: "export" | "import"; detail: string };

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function TransferSettings() {
  const { t } = useTranslation("settings");
  const [state, setState] = useState<Transfer>({ kind: "idle" });
  const busy = state.kind === "working";
  const filters = [{ name: t("transfer.fileType"), extensions: [EXTENSION] }];

  const exportSettings = async () => {
    const path = await chooseSavePath({ title: t("transfer.exportTitle"), defaultPath: `settings.${EXTENSION}`, filters });
    if (!path) return;
    setState({ kind: "working", action: "export" });
    try {
      const summary = await call(commands.exportSettings(path));
      setState({ kind: "exported", path, summary });
    } catch (e) {
      setState({ kind: "failed", action: "export", detail: errorText(e) });
    }
  };

  const chooseImport = async () => {
    const path = await chooseOpenFile(t("transfer.importTitle"), filters);
    if (!path) return;
    setState({ kind: "working", action: "preview" });
    try {
      const summary = await call(commands.previewSettingsImport(path));
      setState({ kind: "preview", path, summary });
    } catch (e) {
      setState({ kind: "failed", action: "import", detail: errorText(e) });
    }
  };

  const importSettings = async (path: string) => {
    setState({ kind: "working", action: "import" });
    try {
      const summary = await call(commands.importSettings(path));
      // What the page shows comes from the core again.
      await Promise.allSettled([usePreferences.getState().load(), useShortcuts.getState().load(), useSnippets.getState().load()]);
      setState({ kind: "imported", summary });
    } catch (e) {
      setState({ kind: "failed", action: "import", detail: errorText(e) });
    }
  };

  return (
    <div className="transfer-settings">
      <p>{t("transfer.hint")}</p>
      <div className="transfer-actions">
        <button disabled={busy} onClick={() => void exportSettings()}>{t("transfer.export")}</button>
        <button disabled={busy} onClick={() => void chooseImport()}>{t("transfer.import")}</button>
      </div>
      <TransferState state={state} onImport={(path) => void importSettings(path)} onCancel={() => setState({ kind: "idle" })} />
    </div>
  );
}

function TransferState({ state, onImport, onCancel }: { state: Transfer; onImport(path: string): void; onCancel(): void }) {
  const { t } = useTranslation("settings");
  switch (state.kind) {
    case "idle":
      return null;
    case "working":
      return <p className="muted small" role="status">{t(`transfer.working.${state.action}`)}</p>;
    case "exported":
      return (
        <p className="small transfer-result" role="status">
          {t("transfer.exported", { path: state.path, total: state.summary.settings + state.summary.shortcuts + state.summary.snippets })}
        </p>
      );
    case "preview":
      return (
        <section className="transfer-preview" aria-labelledby="transfer-preview-title">
          <h3 id="transfer-preview-title">{t("transfer.previewTitle")}</h3>
          <p className="muted small">{t("transfer.from", { version: state.summary.appVersion, at: state.summary.exportedAt })}</p>
          <SummaryList summary={state.summary} />
          <p className="muted small">{t("transfer.replaceNote")}</p>
          <div className="transfer-actions">
            <button onClick={onCancel}>{t("transfer.cancel")}</button>
            <button className="primary" onClick={() => onImport(state.path)}>{t("transfer.importAction")}</button>
          </div>
        </section>
      );
    case "imported":
      return (
        <section className="transfer-result" role="status" aria-labelledby="transfer-imported-title">
          <h3 id="transfer-imported-title">{t("transfer.importedTitle")}</h3>
          <SummaryList summary={state.summary} />
        </section>
      );
    case "failed":
      return <p className="error small" role="alert">{t(`transfer.failed.${state.action}`, { detail: state.detail })}</p>;
    default: {
      const never: never = state;
      return never;
    }
  }
}

function SummaryList({ summary }: { summary: SettingsSummaryItem }) {
  const { t } = useTranslation("settings");
  const lines: string[] = [t("transfer.counts", { settings: summary.settings, shortcuts: summary.shortcuts, snippets: summary.snippets })];
  if (summary.variables.length > 0) lines.push(t("transfer.variables", { names: summary.variables.join(", ") }));
  if (summary.tunnelPorts) lines.push(t("transfer.tunnelPorts", { min: summary.tunnelPorts.min, max: summary.tunnelPorts.max }));
  if (summary.maskingAdded.length > 0) lines.push(t("transfer.maskingAdded", { patterns: summary.maskingAdded.join(", ") }));
  if (summary.typesAdded.length > 0) lines.push(t("transfer.typesAdded", { types: summary.typesAdded.join(", ") }));
  if (summary.typesKept.length > 0) lines.push(t("transfer.typesKept", { types: summary.typesKept.join(", ") }));
  if (summary.mcpReadOnlyKept) lines.push(t("transfer.mcpReadOnlyKept"));
  if (summary.skipped.length > 0) lines.push(t("transfer.skipped", { names: summary.skipped.join(", ") }));
  return (
    <ul className="transfer-summary">
      {lines.map((line) => (
        <li key={line}>{line}</li>
      ))}
    </ul>
  );
}
