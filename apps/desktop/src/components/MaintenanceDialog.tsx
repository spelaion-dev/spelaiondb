// A maintenance action of the explorer (SDB-392): its options, and the statement the core writes
// from them (ADR-026), shown before anything runs. Run opens it in an editor tab of its own and
// runs it there as shown, so the guard asks what it asks of any run (a TRUNCATE), a read-only
// connection refuses it, its output lands in that tab's results and Messages, and VACUUM runs
// outside any transaction (a new tab has none open).

import { useEffect, useState } from "react";
import { type ConnectionKey } from "../connectionKey";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { MaintenanceKindItem, MaintenanceOptionsItem_Deserialize as MaintenanceOptionsItem, MaintenancePlanItem, ObjectTargetItem_Deserialize as ObjectTargetItem } from "../bindings";
import { connectTabFromExplorer, explorerDatabase } from "../connect";
import { useApp } from "../store";

const NO_OPTIONS: MaintenanceOptionsItem = {
  full: false,
  freeze: false,
  analyze: false,
  disablePageSkipping: false,
  skipLocked: false,
  indexCleanup: null,
  truncatePages: null,
  verbose: false,
  noData: false,
  concurrently: false,
  only: false,
  restartIdentity: false,
  cascade: false,
  checkMode: null,
  quick: false,
  extended: false,
  useFrm: false,
  local: false,
};

type Flag = "full" | "freeze" | "analyze" | "disablePageSkipping" | "skipLocked" | "verbose" | "noData" | "concurrently" | "only" | "restartIdentity" | "cascade" | "quick" | "extended" | "useFrm" | "local";

/** The options each action has on each engine (the core reads only these). */
function optionsOf(engine: string, kind: MaintenanceKindItem): { flags: Flag[]; indexCleanup: boolean; truncatePages: boolean; checkMode: boolean } {
  const none = { flags: [] as Flag[], indexCleanup: false, truncatePages: false, checkMode: false };
  const mysql = engine === "mysql" || engine === "mariadb";
  if (engine === "postgres") {
    switch (kind) {
      case "vacuum":
        return { flags: ["full", "freeze", "analyze", "verbose", "disablePageSkipping", "skipLocked"], indexCleanup: true, truncatePages: true, checkMode: false };
      case "analyze":
        return { ...none, flags: ["verbose"] };
      case "refreshMaterializedView":
        return { ...none, flags: ["concurrently", "noData"] };
      case "truncate":
        return { ...none, flags: ["only", "restartIdentity", "cascade"] };
      default:
        return none;
    }
  }
  if (mysql) {
    switch (kind) {
      case "check":
        return { ...none, checkMode: true };
      case "repair":
        return { ...none, flags: ["quick", "extended", "useFrm", "local"] };
      case "analyze":
      case "optimize":
        return { ...none, flags: ["local"] };
      default:
        return none;
    }
  }
  return none;
}

export function MaintenanceDialog({
  session,
  identity,
  engine,
  schema,
  name,
  target,
  kind,
  onClose,
}: {
  session: string;
  identity: ConnectionKey;
  engine: string;
  /** The schema the tab opens in. */
  schema: string;
  /** What it acts on, as the explorer names it. */
  name: string;
  target: ObjectTargetItem;
  kind: MaintenanceKindItem;
  onClose(): void;
}) {
  const { t } = useTranslation("shell");
  const [options, setOptions] = useState<MaintenanceOptionsItem>(NO_OPTIONS);
  const [plan, setPlan] = useState<MaintenancePlanItem | null>(null);
  const [error, setError] = useState<string | null>(null);
  const shown = optionsOf(engine, kind);
  const action = t(`maintenance.kinds.${kind}`);

  // The statement follows the options; an answer to older options is ignored.
  const optionsKey = JSON.stringify(options);
  useEffect(() => {
    let current = true;
    call(commands.maintenancePlan(session, target, kind, options))
      .then((next) => {
        if (!current) return;
        setPlan(next);
        setError(null);
      })
      .catch((e: unknown) => {
        if (!current) return;
        setPlan(null);
        setError(e instanceof Error ? e.message : String(e));
      });
    return () => { current = false; };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [optionsKey]);

  /** A new tab on the connection with the statement; `run` runs it once connected. */
  function openTab(run: boolean) {
    if (!plan) return;
    const app = useApp.getState();
    const tab = app.addTab(identity, plan.sql);
    if (run) app.updateTab(tab, { runOnOpen: true });
    connectTabFromExplorer(tab, session, explorerDatabase(identity, schema));
    onClose();
  }

  return (
    <div className="modal-backdrop">
      <form
        className="modal object-change-dialog maintenance-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={t("maintenance.title", { action, name })}
        onSubmit={(e) => {
          e.preventDefault();
          openTab(true);
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") { e.stopPropagation(); onClose(); }
        }}
      >
        <h2>{t("maintenance.title", { action, name })}</h2>
        {(shown.flags.length > 0 || shown.indexCleanup || shown.truncatePages || shown.checkMode) && (
          <fieldset className="maintenance-options">
            <legend className="muted small">{t("maintenance.options")}</legend>
            {shown.flags.map((flag) => (
              <label key={flag} className="check">
                <input type="checkbox" name={flag} checked={options[flag]} onChange={(e) => setOptions({ ...options, [flag]: e.target.checked })} />
                <span>{t(`maintenance.flags.${flag}`)}</span>
              </label>
            ))}
            {shown.indexCleanup && (
              <label className="field">
                <span>{t("maintenance.indexCleanup")}</span>
                <select value={options.indexCleanup ?? ""} onChange={(e) => setOptions({ ...options, indexCleanup: e.target.value === "" ? null : (e.target.value as MaintenanceOptionsItem["indexCleanup"]) })}>
                  <option value="">{t("maintenance.serverDefault")}</option>
                  <option value="auto">AUTO</option>
                  <option value="on">ON</option>
                  <option value="off">OFF</option>
                </select>
              </label>
            )}
            {shown.truncatePages && (
              <label className="field">
                <span>{t("maintenance.truncatePages")}</span>
                <select value={options.truncatePages === null ? "" : String(options.truncatePages)} onChange={(e) => setOptions({ ...options, truncatePages: e.target.value === "" ? null : e.target.value === "true" })}>
                  <option value="">{t("maintenance.serverDefault")}</option>
                  <option value="true">{t("maintenance.truncateYes")}</option>
                  <option value="false">{t("maintenance.truncateNo")}</option>
                </select>
              </label>
            )}
            {shown.checkMode && (
              <label className="field">
                <span>{t("maintenance.checkMode")}</span>
                <select value={options.checkMode ?? ""} onChange={(e) => setOptions({ ...options, checkMode: e.target.value === "" ? null : (e.target.value as MaintenanceOptionsItem["checkMode"]) })}>
                  <option value="">{t("maintenance.serverDefault")}</option>
                  <option value="quick">QUICK</option>
                  <option value="fast">FAST</option>
                  <option value="medium">MEDIUM</option>
                  <option value="extended">EXTENDED</option>
                  <option value="changed">CHANGED</option>
                </select>
              </label>
            )}
          </fieldset>
        )}
        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono" aria-label={t("objectChange.sql")}>
          {plan?.sql ?? (error ? "" : "…")}
        </pre>
        {plan && plan.alsoEmpties.length > 0 && (
          <p className="warn small">{t("maintenance.alsoEmpties", { tables: plan.alsoEmpties.join(", ") })}</p>
        )}
        {plan?.outsideTransaction && <p className="muted small">{t("maintenance.outsideTransaction")}</p>}
        <p className="muted small">{t("maintenance.runsInTab")}</p>
        {error && (
          <p className="error small" role="alert">
            {t("maintenance.failed", { detail: error })}
          </p>
        )}
        <div className="modal-actions">
          <button type="button" onClick={() => openTab(false)} disabled={!plan}>
            {t("objectChange.openInEditor")}
          </button>
          <span className="spacer" />
          <button type="button" onClick={onClose}>
            {t("cancel")}
          </button>
          <button type="submit" className="primary" disabled={!plan} autoFocus>
            {t("maintenance.run")}
          </button>
        </div>
      </form>
    </div>
  );
}
