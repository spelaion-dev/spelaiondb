// Running an .sql file against a connection (ADR-049): the core reads the file, counts its
// statements and what the guard would ask about, and runs it in a session of its own after the
// user confirms. This dialog shows the plan, asks, and reports the statements done of the total,
// the elapsed time and any failure; the core refuses a read-only connection and an unconfirmed run.

import { useEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands } from "../api";
import type { BackupEventItem, FilePlanItem, OnErrorItem } from "../bindings";
import { chooseOpenFile, openFilters } from "../dialogs";
import { formatBytes } from "./ServerObservation";

export interface RunFileRequest {
  /** An open session of the connection (the explorer's): the run opens its own from it. */
  session: string;
  connectionName: string;
}

export const useRunFile = create<{ request: RunFileRequest | null; open(request: RunFileRequest): void; close(): void }>((set) => ({
  request: null,
  open: (request) => set({ request }),
  close: () => set({ request: null }),
}));

type Phase =
  | { kind: "idle" }
  | { kind: "running"; done: number; total: number }
  | { kind: "done"; text: string }
  | { kind: "failed"; text: string };

/** The failure lines kept: the newest. */
const LOG_LINES = 500;

let nextRun = Math.floor(Math.random() * 1_000_000);

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function RunFileDialog() {
  const request = useRunFile((s) => s.request);
  if (!request) return null;
  return <RunFileForm key={request.session} request={request} />;
}

function RunFileForm({ request }: { request: RunFileRequest }) {
  const { t, i18n } = useTranslation("editor");
  const close = useRunFile((s) => s.close);
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const seconds = useMemo(() => new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }), [i18n.language]);
  const [path, setPath] = useState("");
  const [onError, setOnError] = useState<OnErrorItem>("stop");
  const [plan, setPlan] = useState<FilePlanItem | null>(null);
  const [planError, setPlanError] = useState<string | null>(null);
  const [confirming, setConfirming] = useState(false);
  const [phase, setPhase] = useState<Phase>({ kind: "idle" });
  const [log, setLog] = useState<string[]>([]);
  const [runId, setRunId] = useState<number | null>(null);
  const [started, setStarted] = useState<number | null>(null);
  const [now, setNow] = useState(Date.now());
  const running = phase.kind === "running";
  const keepRef = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || running) return;
      if (confirming) setConfirming(false);
      else close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, confirming, close]);

  // The elapsed time, once a second while it runs.
  useEffect(() => {
    if (!running) return;
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [running]);

  useEffect(() => {
    if (confirming) keepRef.current?.focus();
  }, [confirming]);

  // The core reads the file again whenever it changes.
  useEffect(() => {
    setPlan(null);
    setPlanError(null);
    if (!path.trim()) return;
    let current = true;
    const timer = window.setTimeout(() => {
      void call(commands.planSqlFile(request.session, path))
        .then((p) => current && setPlan(p))
        .catch((e: unknown) => current && setPlanError(message(e)));
    }, 250);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [path, request.session]);

  async function pick() {
    const chosen = await chooseOpenFile(
      t("runFile.chooseFile"),
      openFilters([
        { name: t("runFile.sqlFiles"), extensions: ["sql"] },
        { name: t("restoreDb.allFiles"), extensions: ["*"] },
      ]) ?? [],
      path || null,
    );
    if (chosen) setPath(chosen);
  }

  function onEvent(event: BackupEventItem) {
    switch (event.kind) {
      case "statements":
        setPhase((p) => (p.kind === "running" ? { kind: "running", done: event.done ?? 0, total: event.total ?? 0 } : p));
        break;
      case "log":
        setLog((lines) => [...lines, event.line].slice(-LOG_LINES));
        break;
      case "started":
      case "progress":
        break;
    }
  }

  async function run() {
    setConfirming(false);
    const id = ++nextRun;
    setRunId(id);
    setLog([]);
    const begin = Date.now();
    setStarted(begin);
    setNow(begin);
    setPhase({ kind: "running", done: 0, total: plan?.statements ?? 0 });
    const channel = new Channel<BackupEventItem>();
    channel.onmessage = onEvent;
    try {
      const outcome = await call(commands.runSqlFile(request.session, id, path, onError, true, channel));
      switch (outcome.kind) {
        case "done":
          setPhase({
            kind: "done",
            text:
              (outcome.failed ?? 0) > 0
                ? t("runFile.doneWithFailures", {
                    done: numbers.format(outcome.done ?? 0),
                    failed: numbers.format(outcome.failed ?? 0),
                    seconds: seconds.format((outcome.millis ?? 0) / 1000),
                  })
                : t("runFile.done", { done: numbers.format(outcome.done ?? 0), seconds: seconds.format((outcome.millis ?? 0) / 1000) }),
          });
          break;
        case "cancelled":
          setPhase({ kind: "failed", text: t("runFile.cancelled") });
          break;
        case "needsConfirmation":
          setPhase({ kind: "failed", text: t("runFile.failed", { reason: t("runFile.notConfirmed") }) });
          break;
        case "failed":
          setPhase({ kind: "failed", text: t("runFile.failed", { reason: outcome.message }) });
          break;
      }
    } catch (e) {
      setPhase({ kind: "failed", text: t("runFile.failed", { reason: message(e) }) });
    } finally {
      setRunId(null);
    }
  }

  const title = t("runFile.title", { name: request.connectionName });
  const elapsed = started === null ? 0 : Math.max(0, (now - started) / 1000);
  const fileName = path.split(/[\\/]/).pop() ?? path;

  return (
    <div className="modal-backdrop">
      <div className="modal backup-dialog" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>
        <p className="muted small">{t("runFile.intro")}</p>
        <fieldset disabled={running}>
          <legend>{t("runFile.file")}</legend>
          <div className="backup-row">
            <input type="text" aria-label={t("runFile.file")} placeholder={t("runFile.file")} value={path} onChange={(e) => setPath(e.target.value)} />
            <button type="button" onClick={() => void pick()}>
              {t("restoreDb.chooseFileButton")}
            </button>
          </div>
          <label className="field">
            {t("runFile.onError")}
            <select value={onError} onChange={(e) => setOnError(e.target.value as OnErrorItem)}>
              <option value="stop">{t("runFile.onErrorStop")}</option>
              <option value="continue">{t("runFile.onErrorContinue")}</option>
            </select>
          </label>
          {plan && (
            <p className="small" role="status">
              {t("runFile.plan", { count: plan.statements ?? 0, formattedCount: numbers.format(plan.statements ?? 0), size: formatBytes(plan.bytes ?? 0, i18n.language) })}
            </p>
          )}
          {planError && (
            <p className="error small" role="alert">
              {t("runFile.cannotRead", { reason: planError })}
            </p>
          )}
        </fieldset>

        {(log.length > 0 || running) && (
          <pre className="backup-log" role="log" aria-live="polite" aria-label={t("runFile.log")} tabIndex={0}>
            {log.join("\n")}
          </pre>
        )}
        {phase.kind === "running" && (
          <div className="backup-progress" role="status">
            <progress max={Math.max(1, phase.total)} value={phase.done} aria-label={t("runFile.progressLabel")} />
            <span>
              {t("runFile.progress", { done: numbers.format(phase.done), total: numbers.format(phase.total), seconds: numbers.format(Math.floor(elapsed)) })}
            </span>
          </div>
        )}
        {phase.kind === "done" && (
          <p role="status" className="backup-result">
            {phase.text}
          </p>
        )}
        {phase.kind === "failed" && (
          <p role="alert" className="error backup-result">
            {phase.text}
          </p>
        )}

        <div className="modal-actions">
          {running ? (
            <button disabled={runId === null} onClick={() => runId !== null && void call(commands.cancelBackup(runId)).catch(() => undefined)}>
              {t("runFile.cancel")}
            </button>
          ) : (
            <>
              <button onClick={close}>{t("backup.close")}</button>
              <button className="primary" disabled={!plan || (plan.statements ?? 0) === 0} onClick={() => setConfirming(true)}>
                {t("runFile.start")}
              </button>
            </>
          )}
        </div>
      </div>

      {confirming && plan && (
        <div className="modal-backdrop">
          <div className="modal backup-confirm" role="dialog" aria-modal="true" aria-label={t("runFile.confirmTitle", { file: fileName })}>
            <h2>{t("runFile.confirmTitle", { file: fileName })}</h2>
            <p>
              {t("runFile.confirmBody", {
                count: plan.statements ?? 0,
                formattedCount: numbers.format(plan.statements ?? 0),
                connection: request.connectionName,
              })}
            </p>
            {plan.risks.length > 0 && (
              <ul className="banner" aria-label={t("runFile.risks")}>
                {plan.risks.map((r) => (
                  <li key={r.risk}>{t(`runFile.riskKinds.${r.risk}`, { count: r.count ?? 0, formattedCount: numbers.format(r.count ?? 0) })}</li>
                ))}
              </ul>
            )}
            {plan.readOnly && (
              <p className="error small" role="alert">
                {t("runFile.readOnly")}
              </p>
            )}
            <div className="modal-actions">
              <button ref={keepRef} autoFocus onClick={() => setConfirming(false)}>
                {t("restoreDb.keep")}
              </button>
              <button className="danger" disabled={plan.readOnly} onClick={() => void run()}>
                {t("runFile.confirm")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
