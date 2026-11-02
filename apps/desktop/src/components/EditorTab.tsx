// An editor tab: its own session in the core (ADR-004), editor, execution and results.

import { useEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { call, commands, type ScriptEvent } from "../api";
import { openSession } from "../connect";
import { useApp, type RunState, type Tab, type Translation } from "../store";
import { byteToIndex } from "../text";
import { ExportButton } from "./ExportButton";
import { ResultGrid } from "./ResultGrid";
import { SqlEditor, type RunRequest } from "./SqlEditor";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

function translated(text: Translation, t: TFunction<"editor">, numbers: Intl.NumberFormat): string {
  const values: Record<string, string | number> = {};
  for (const [key, value] of Object.entries(text.values ?? {})) {
    values[key] = typeof value === "number" && key !== "count" ? numbers.format(value) : value;
  }
  if (typeof values.count === "number") values.formattedCount = numbers.format(values.count);
  return t(text.key, values);
}

function translatedError(error: NonNullable<RunState["error"]>, t: TFunction<"editor">, numbers: Intl.NumberFormat): string {
  const diagnostic = error.diagnostic;
  if (!diagnostic) return error.translation ? translated(error.translation, t, numbers) : error.message;
  const parts = [diagnostic.cancelled ? t("messages.cancelled") : diagnostic.message];
  if (diagnostic.detail) parts.push(t("messages.detail", { detail: diagnostic.detail }));
  if (diagnostic.hint) parts.push(t("messages.hint", { hint: diagnostic.hint }));
  if (diagnostic.code) parts.push(t("messages.code", { code: diagnostic.code }));
  return parts.join("\n");
}

export function EditorTab({ tab }: { tab: Tab }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const connections = useApp((s) => s.connections);
  const updateTab = useApp((s) => s.updateTab);
  const updateRun = useApp((s) => s.updateRun);
  const historySaving = useApp((s) => tab.connectionId !== null && s.historySaving.includes(tab.connectionId));
  const workspaceChanging = useApp((s) => s.workspaceChanging);
  const connection = connections.find((c) => c.id === tab.connectionId) ?? null;
  const [tables, setTables] = useState<Record<string, string[]>>({});
  const [editorShare, setEditorShare] = useState(0.42);
  const body = useRef<HTMLDivElement>(null);
  const lastRequest = useRef<RunRequest | null>(null);

  // Autocompletion: tables and views of the user schemas.
  useEffect(() => {
    if (tab.status !== "connected") return;
    let cancelled = false;
    (async () => {
      const schemas = (await call(commands.listSchemas(tab.id))).filter((s) => !s.system).slice(0, 20);
      const out: Record<string, string[]> = {};
      for (const schema of schemas) {
        for (const o of await call(commands.listObjects(tab.id, schema.name))) {
          out[`${o.schema}.${o.name}`] = [];
          if (o.schema === "public" || o.schema === "main") out[o.name] = [];
        }
      }
      if (!cancelled) setTables(out);
    })().catch(() => undefined);
    return () => {
      cancelled = true;
    };
  }, [tab.status, tab.id]);

  async function ensureConnected(): Promise<boolean> {
    if (tab.status === "connected") return true;
    if (!tab.connectionId) {
      updateTab(tab.id, { run: errorRun("", { key: "pickConnection" }) });
      return false;
    }
    updateTab(tab.id, { status: "connecting" });
    try {
      const version = await openSession(tab.id, tab.connectionId);
      if (version === null) {
        updateTab(tab.id, { status: "disconnected" });
        return false;
      }
      updateTab(tab.id, { status: "connected", serverVersion: version });
      return true;
    } catch (e) {
      const detail = message(e);
      updateTab(tab.id, { status: "disconnected", run: errorRun(detail, { key: "messages.connectionFailed", values: { detail } }) });
      return false;
    }
  }

  async function changeConnection(id: string) {
    await call(commands.disconnect(tab.id)).catch(() => undefined);
    setTables({});
    updateTab(tab.id, { connectionId: id || null, status: "disconnected", serverVersion: null });
  }

  async function run(request: RunRequest) {
    if (!request.text.trim() || tab.run?.running || useApp.getState().workspaceChanging ||
        (tab.connectionId && useApp.getState().historySaving.includes(tab.connectionId))) return;
    if (!(await ensureConnected())) return;
    for (const r of tab.run?.results ?? []) void call(commands.releaseResult(r.id)).catch(() => undefined);
    lastRequest.current = request;
    const startedAt = performance.now();
    updateTab(tab.id, {
      run: { running: true, results: [], activeResult: null, messages: [], error: null, startedAt, elapsedMs: null },
    });
    const channel = new Channel<ScriptEvent>();
    channel.onmessage = (event) => handle(event, request, startedAt);
    try {
      await call(commands.runScript(tab.id, request.text, channel));
    } catch (e) {
      const detail = message(e);
      updateRun(tab.id, (r) => ({ ...r, running: false, error: { message: detail, offset: null, translation: { key: "messages.runFailed", values: { detail } } } }));
    }
  }

  function handle(event: ScriptEvent, request: RunRequest, startedAt: number) {
    switch (event.kind) {
      case "statementStarted":
        break;
      case "resultStarted":
        updateRun(tab.id, (r) => ({
          ...r,
          results: [...r.results, { id: event.result, statement: event.index, columns: event.columns, rows: 0, complete: false }],
          activeResult: r.activeResult ?? event.result,
        }));
        break;
      case "rows":
        updateRun(tab.id, (r) => ({
          ...r,
          results: r.results.map((x) => (x.id === event.result ? { ...x, rows: event.totalRows } : x)),
        }));
        break;
      case "notice":
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: `${event.severity}: ${event.message}` }] }));
        break;
      case "historyWarning":
        updateRun(tab.id, (r) => ({ ...r, messages: [...r.messages, { kind: "notice", text: event.message, translation: { key: "messages.historySaveFailed", values: { detail: event.message } } }] }));
        break;
      case "statementFinished": {
        const ms = Math.round(event.elapsedMs ?? 0);
        const translation: Translation = event.rowsAffected == null
          ? { key: "messages.statementOk", values: { index: event.index + 1, duration: ms } }
          : { key: "messages.statementRows", values: { index: event.index + 1, count: event.rowsAffected, duration: ms } };
        updateRun(tab.id, (r) => ({
          ...r,
          results: r.results.map((x) => (x.statement === event.index ? { ...x, complete: true } : x)),
          messages: [...r.messages, { kind: "info", text: "", translation }],
        }));
        break;
      }
      case "statementFailed": {
        const offset =
          event.scriptOffset == null ? null : request.from + byteToIndex(request.text, event.scriptOffset);
        const diagnostic = { message: event.message, detail: event.detail, hint: event.hint, code: event.code, cancelled: event.cancelled };
        updateRun(tab.id, (r) => ({
          ...r,
          error: { message: event.message, offset, diagnostic },
          messages: [...r.messages, { kind: "error", text: event.message, translation: { key: event.cancelled ? "messages.statementCancelled" : "messages.statementFailed", values: { index: event.index + 1, reason: event.message } } }],
        }));
        break;
      }
      case "scriptFinished":
        updateRun(tab.id, (r) => ({ ...r, running: false, elapsedMs: performance.now() - startedAt }));
        useApp.getState().refreshHistory();
        break;
    }
  }

  const editorError = useMemo(
    () => (tab.run?.error?.offset != null ? { index: tab.run.error.offset, message: translatedError(tab.run.error, t, numbers), source: tab.run.error } : null),
    [tab.run?.error, t, numbers],
  );

  function startDrag(e: React.PointerEvent) {
    const el = body.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const move = (ev: PointerEvent) =>
      setEditorShare(Math.min(0.85, Math.max(0.12, (ev.clientY - rect.top) / rect.height)));
    const up = () => {
      window.removeEventListener("pointermove", move);
      window.removeEventListener("pointerup", up);
    };
    window.addEventListener("pointermove", move);
    window.addEventListener("pointerup", up);
    e.preventDefault();
  }

  const running = tab.run?.running ?? false;
  return (
    <div className="editor-tab">
      <div className="toolbar">
        <select value={tab.connectionId ?? ""} onChange={(e) => void changeConnection(e.target.value)} aria-label={t("connection")} disabled={running || tab.status === "connecting" || historySaving || workspaceChanging}>
          <option value="">{t("noConnection")}</option>
          {connections.map((c) => (
            <option key={c.id} value={c.id}>
              {c.name} ({c.engine}
              {c.env ? `, ${c.env}` : ""})
            </option>
          ))}
        </select>
        <span className={`status status-${tab.status}`}>
          {t(`status.${tab.status}`)}{tab.status === "connected" && tab.serverVersion ? ` · ${tab.serverVersion}` : ""}
        </span>
        {connection?.env?.toLowerCase().startsWith("prod") && <span className="badge badge-prod">{t("production")}</span>}
        <div className="spacer" />
        {running ? (
          <button className="danger" onClick={() => void call(commands.cancel(tab.id)).catch(() => undefined)}>
            {t("cancel")}
          </button>
        ) : (
          <button
            className="primary"
            disabled={historySaving || tab.status === "connecting" || workspaceChanging}
            title={t("runHint")}
            onClick={() => void run({ text: tab.sql, from: 0 })}
          >
            {t("run")}
          </button>
        )}
      </div>
      <div className="editor-body" ref={body}>
        <div style={{ flex: `${editorShare} 1 0` }} className="editor-pane">
          <SqlEditor
            value={tab.sql}
            onChange={(sql) => updateTab(tab.id, { sql })}
            engine={connection?.engine ?? null}
            tables={tables}
            onRun={(r) => void run(r)}
            error={editorError}
          />
        </div>
        <div className="divider" onPointerDown={startDrag} role="separator" aria-orientation="horizontal" aria-label={t("resizeEditor")} />
        <div style={{ flex: `${1 - editorShare} 1 0` }} className="results-pane">
          <Results run={tab.run} session={tab.id} onSelect={(id) => updateRun(tab.id, (r) => ({ ...r, activeResult: id }))} />
        </div>
      </div>
    </div>
  );
}

function errorRun(text: string, translation?: Translation): RunState {
  return {
    running: false,
    results: [],
    activeResult: null,
    messages: [{ kind: "error", text, translation }],
    error: { message: text, offset: null, translation },
    startedAt: 0,
    elapsedMs: null,
  };
}

function Results({ run, session, onSelect }: { run: RunState | null; session: string; onSelect(id: number | null): void }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  if (!run) return <div className="empty">{t("results.empty")}</div>;
  const active = run.results.find((r) => r.id === run.activeResult) ?? null;
  const showMessages = active === null;
  const error = run.error ? translatedError(run.error, t, numbers) : null;
  return (
    <div className="results">
      <div className="result-tabs">
        {run.results.map((r, i) => (
          <button key={r.id} className={r.id === run.activeResult ? "active" : ""} onClick={() => onSelect(r.id)}>
            {t("results.result", { number: numbers.format(i + 1) })}
          </button>
        ))}
        <button className={showMessages ? "active" : ""} onClick={() => onSelect(null)}>
          {t("results.messages")}{run.messages.some((m) => m.kind === "error") ? " ⚠" : ""}
        </button>
        {active && <ExportButton key={active.id} result={active} session={session} />}
      </div>
      <div className="result-body">
        {active ? (
          <ResultGrid result={active} session={session} />
        ) : (
          <div className="messages">
            {error && <pre className="error-box">{error}</pre>}
            {run.messages.map((m, i) => (
              <div key={i} className={`message message-${m.kind}`}>
                {m.translation ? translated(m.translation, t, numbers) : m.text}
              </div>
            ))}
          </div>
        )}
      </div>
      <div className="statusline">
        {run.running ? (
          <span>{t("results.running")}</span>
        ) : (
          <span>
            {active ? `${t("results.rows", { count: active.rows, formattedCount: numbers.format(active.rows) })}${active.complete ? "" : t("results.loading")} · ` : ""}
            {run.elapsedMs != null ? t("results.duration", { duration: numbers.format(Math.round(run.elapsedMs)) }) : ""}
          </span>
        )}
        {error && !showMessages && <span className="status-error">{t("results.error", { message: error.split("\n")[0] })}</span>}
      </div>
    </div>
  );
}
