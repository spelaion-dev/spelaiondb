// Settings › Connections: moving personal connections to another computer in a file encrypted
// with a password (ADR-088). The core gathers, seals, opens, checks and writes everything, the
// passwords included (`connection_transfer.rs`); the page chooses the connections and the file,
// takes the password, shows what an import would do before anything is written, and shows what
// it did. Team connections are not here: they move with their repository (ADR-022).

import type { TFunction } from "i18next";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { ConnectionImportItem, ConnectionImportResultItem, ConnectionsExportedItem } from "../bindings";
import { errorText } from "../dialogState";
import { chooseOpenFile, chooseSavePath } from "../dialogs";
import { useApp } from "../store";
import { refreshConnections } from "./Sidebar";

const EXTENSION = "spelaion-connections";
/** The core refuses a shorter one too (`EXPORT_PASSWORD_MIN`); here it only explains early. */
const PASSWORD_MIN = 12;

/** The export form and its result: only the page has this state (ADR-060). */
type ExportState =
  | { kind: "closed" }
  | { kind: "form"; error: string | null }
  | { kind: "exporting" }
  | { kind: "done"; path: string; result: ConnectionsExportedItem };

/** The import's steps: only the page has this state. */
type ImportState =
  | { kind: "closed" }
  | { kind: "password"; path: string; error: string | null }
  | { kind: "opening"; path: string }
  | { kind: "preview"; path: string; items: ConnectionImportItem[] }
  | { kind: "importing"; path: string }
  | { kind: "done"; results: ConnectionImportResultItem[] };

export function ConnectionTransferSettings() {
  const { t } = useTranslation("settings");
  return (
    <section aria-labelledby="connection-transfer-title">
      <h3 id="connection-transfer-title">{t("connectionTransfer.title")}</h3>
      <p>{t("connectionTransfer.hint")}</p>
      <ExportConnections />
      <ImportConnections />
    </section>
  );
}

function ExportConnections() {
  const { t } = useTranslation("settings");
  const connections = useApp((s) => s.connections);
  const personal = connections.filter((connection) => connection.personal);
  const [state, setState] = useState<ExportState>({ kind: "closed" });
  const [chosen, setChosen] = useState<Set<string> | null>(null);
  const [password, setPassword] = useState("");
  const [again, setAgain] = useState("");
  const selected = chosen ?? new Set(personal.map((connection) => connection.id));

  const toggle = (id: string) => {
    const next = new Set(selected);
    if (next.has(id)) next.delete(id);
    else next.add(id);
    setChosen(next);
  };

  const exportNow = async () => {
    if (password.length < PASSWORD_MIN) {
      setState({ kind: "form", error: t("connectionTransfer.passwordShort", { count: PASSWORD_MIN }) });
      return;
    }
    if (password !== again) {
      setState({ kind: "form", error: t("connectionTransfer.passwordsDiffer") });
      return;
    }
    const path = await chooseSavePath({
      title: t("connectionTransfer.exportTitle"),
      defaultPath: `connections.${EXTENSION}`,
      filters: [{ name: t("connectionTransfer.fileType"), extensions: [EXTENSION] }],
    });
    if (!path) return;
    setState({ kind: "exporting" });
    try {
      const result = await call(commands.exportPersonalConnections([...selected], password, path));
      setPassword("");
      setAgain("");
      setState({ kind: "done", path, result });
    } catch (e) {
      setState({ kind: "form", error: t("connectionTransfer.exportFailed", { detail: errorText(e) }) });
    }
  };

  switch (state.kind) {
    case "closed":
      return (
        <div className="connection-transfer-actions">
          <button disabled={personal.length === 0} title={personal.length === 0 ? t("connectionTransfer.noPersonal") : undefined} onClick={() => setState({ kind: "form", error: null })}>
            {t("connectionTransfer.export")}
          </button>
        </div>
      );
    case "form":
    case "exporting": {
      const busy = state.kind === "exporting";
      return (
        <fieldset className="connection-transfer-form" aria-label={t("connectionTransfer.exportLegend")}>
          <legend>{t("connectionTransfer.exportLegend")}</legend>
          <ul className="connection-transfer-list" aria-label={t("connectionTransfer.chooseConnections")}>
            {personal.map((connection) => (
              <li key={connection.id}>
                <label className="check">
                  <input type="checkbox" checked={selected.has(connection.id)} disabled={busy} onChange={() => toggle(connection.id)} />
                  {connection.name}
                  {connection.tunnel && <span className="muted small"> · {t("connectionTransfer.through", { profile: connection.tunnel })}</span>}
                </label>
              </li>
            ))}
          </ul>
          <label className="field">
            <span>{t("connectionTransfer.password")}</span>
            <input type="password" autoComplete="new-password" value={password} disabled={busy} onChange={(e) => setPassword(e.target.value)} />
          </label>
          <label className="field">
            <span>{t("connectionTransfer.passwordAgain")}</span>
            <input type="password" autoComplete="new-password" value={again} disabled={busy} onChange={(e) => setAgain(e.target.value)} />
          </label>
          <p className="muted small">{t("connectionTransfer.exportWarning", { count: PASSWORD_MIN })}</p>
          {state.kind === "form" && state.error && <p className="error small" role="alert">{state.error}</p>}
          <div className="connection-transfer-actions">
            <button disabled={busy} onClick={() => setState({ kind: "closed" })}>{t("connectionTransfer.cancel")}</button>
            <button className="primary" disabled={busy || selected.size === 0} onClick={() => void exportNow()}>
              {busy ? t("connectionTransfer.exporting") : t("connectionTransfer.exportAction")}
            </button>
          </div>
        </fieldset>
      );
    }
    case "done":
      return (
        <div className="connection-transfer-result" role="status">
          <p className="small">
            {t("connectionTransfer.exported", {
              connections: state.result.connections,
              profiles: state.result.networkProfiles,
              secrets: state.result.secrets,
              path: state.path,
            })}
          </p>
          {state.result.withoutKeychain && <p className="small">{t("connectionTransfer.withoutKeychain")}</p>}
          {state.result.profilesNotCarried.length > 0 && (
            <p className="small">{t("connectionTransfer.profilesNotCarried", { profiles: state.result.profilesNotCarried.join(", ") })}</p>
          )}
          <p className="muted small">{t("connectionTransfer.deleteAfter")}</p>
          <div className="connection-transfer-actions">
            <button onClick={() => setState({ kind: "closed" })}>{t("connectionTransfer.close")}</button>
          </div>
        </div>
      );
    default: {
      const never: never = state;
      return never;
    }
  }
}

function itemState(t: TFunction<"settings">, item: ConnectionImportItem): string {
  switch (item.state.kind) {
    case "new":
      return t("connectionTransfer.state.new");
    case "exists":
      return t("connectionTransfer.state.exists");
    case "refused":
      return t("connectionTransfer.state.refused", { reason: item.state.reason });
    case "invalid":
      return t("connectionTransfer.state.invalid", { reason: item.state.reason });
    default: {
      const never: never = item.state;
      return never;
    }
  }
}

function outcomeText(t: TFunction<"settings">, result: ConnectionImportResultItem): string {
  switch (result.outcome.kind) {
    case "added":
      return t("connectionTransfer.outcome.added");
    case "replaced":
      return t("connectionTransfer.outcome.replaced");
    case "skipped":
      return t("connectionTransfer.outcome.skipped");
    case "notImported":
      return t("connectionTransfer.outcome.notImported", { reason: result.outcome.reason });
    case "failed":
      return t("connectionTransfer.outcome.failed", { reason: result.outcome.reason });
    default: {
      const never: never = result.outcome;
      return never;
    }
  }
}

function ImportConnections() {
  const { t } = useTranslation("settings");
  const [state, setState] = useState<ImportState>({ kind: "closed" });
  const [password, setPassword] = useState("");
  const [replace, setReplace] = useState<Set<string>>(new Set());
  const keyOf = (item: { kind: string; id: string }) => `${item.kind}:${item.id}`;

  const choose = async () => {
    const path = await chooseOpenFile(t("connectionTransfer.importTitle"), [{ name: t("connectionTransfer.fileType"), extensions: [EXTENSION] }]);
    if (!path) return;
    setPassword("");
    setState({ kind: "password", path, error: null });
  };

  const open = async (path: string) => {
    setState({ kind: "opening", path });
    try {
      const items = await call(commands.previewConnectionsFile(path, password));
      setReplace(new Set());
      setState({ kind: "preview", path, items });
    } catch (e) {
      setState({ kind: "password", path, error: t("connectionTransfer.openFailed", { detail: errorText(e) }) });
    }
  };

  const importNow = async (path: string, items: ConnectionImportItem[]) => {
    const chosen = (kind: ConnectionImportItem["kind"]) => items.filter((item) => item.kind === kind && replace.has(keyOf(item))).map((item) => item.id);
    setState({ kind: "importing", path });
    try {
      const results = await call(commands.importConnectionsFile(path, password, chosen("connection"), chosen("networkProfile")));
      setPassword("");
      await refreshConnections();
      setState({ kind: "done", results });
    } catch (e) {
      setState({ kind: "password", path, error: t("connectionTransfer.importFailed", { detail: errorText(e) }) });
    }
  };

  switch (state.kind) {
    case "closed":
      return (
        <div className="connection-transfer-actions">
          <button onClick={() => void choose()}>{t("connectionTransfer.import")}</button>
        </div>
      );
    case "password":
    case "opening": {
      const busy = state.kind === "opening";
      return (
        <form
          className="connection-transfer-form"
          aria-label={t("connectionTransfer.passwordFor")}
          onSubmit={(e) => {
            e.preventDefault();
            void open(state.path);
          }}
        >
          <p className="small connection-transfer-result">{t("connectionTransfer.file", { path: state.path })}</p>
          <label className="field">
            <span>{t("connectionTransfer.filePassword")}</span>
            <input type="password" autoComplete="off" autoFocus value={password} disabled={busy} onChange={(e) => setPassword(e.target.value)} />
          </label>
          {state.kind === "password" && state.error && <p className="error small" role="alert">{state.error}</p>}
          <div className="connection-transfer-actions">
            <button type="button" disabled={busy} onClick={() => setState({ kind: "closed" })}>{t("connectionTransfer.cancel")}</button>
            <button type="submit" className="primary" disabled={busy || password.length === 0}>
              {busy ? t("connectionTransfer.opening") : t("connectionTransfer.open")}
            </button>
          </div>
        </form>
      );
    }
    case "preview":
    case "importing": {
      const busy = state.kind === "importing";
      const items = state.kind === "preview" ? state.items : [];
      return (
        <section className="connection-transfer-form" aria-labelledby="connection-import-title">
          <h3 id="connection-import-title">{t("connectionTransfer.previewTitle")}</h3>
          <ul className="connection-transfer-list">
            {items.map((item) => (
              <li key={keyOf(item)} data-state={item.state.kind}>
                <span className="connection-transfer-name">
                  {t(`connectionTransfer.kind.${item.kind}`)}: {item.name}
                  {item.secrets > 0 && <span className="muted small"> · {t("connectionTransfer.passwords", { count: item.secrets })}</span>}
                </span>
                <span className="muted small">{itemState(t, item)}</span>
                {item.state.kind === "exists" && (
                  <label className="check small">
                    <input
                      type="checkbox"
                      checked={replace.has(keyOf(item))}
                      disabled={busy}
                      onChange={() => {
                        const next = new Set(replace);
                        if (next.has(keyOf(item))) next.delete(keyOf(item));
                        else next.add(keyOf(item));
                        setReplace(next);
                      }}
                    />
                    {t("connectionTransfer.replace")}
                  </label>
                )}
              </li>
            ))}
          </ul>
          <p className="muted small">{t("connectionTransfer.importNote")}</p>
          <div className="connection-transfer-actions">
            <button disabled={busy} onClick={() => setState({ kind: "closed" })}>{t("connectionTransfer.cancel")}</button>
            <button
              className="primary"
              disabled={busy || !items.some((item) => item.state.kind === "new" || replace.has(keyOf(item)))}
              onClick={() => state.kind === "preview" && void importNow(state.path, state.items)}
            >
              {busy ? t("connectionTransfer.importing") : t("connectionTransfer.importAction")}
            </button>
          </div>
        </section>
      );
    }
    case "done":
      return (
        <section className="connection-transfer-result" role="status" aria-labelledby="connection-imported-title">
          <h3 id="connection-imported-title">{t("connectionTransfer.importedTitle")}</h3>
          <ul className="connection-transfer-list">
            {state.results.map((result) => (
              <li key={keyOf(result)} data-outcome={result.outcome.kind}>
                <span className="connection-transfer-name">{t(`connectionTransfer.kind.${result.kind}`)}: {result.id}</span>
                <span className="muted small">{outcomeText(t, result)}</span>
              </li>
            ))}
          </ul>
          <p className="muted small">{t("connectionTransfer.deleteAfter")}</p>
          <div className="connection-transfer-actions">
            <button onClick={() => setState({ kind: "closed" })}>{t("connectionTransfer.close")}</button>
          </div>
        </section>
      );
    default: {
      const never: never = state;
      return never;
    }
  }
}
