// The Tasks view (ADR-089): the exports, imports, backups and SQL file runs saved with their
// options, by folder, each with Run, Edit…, Duplicate and Delete…, and its runs from the app and
// from the command line with their logs. A thin adapter: the core keeps the tasks, decides
// whether a run asks first and writes the plan it shows, runs it and records it. Nothing here
// starts a task on its own.

import { Channel } from "@tauri-apps/api/core";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { TaskEventItem, TaskItem, TaskKindItem, TaskLogItem, TaskOutcomeItem, TaskPlanItem, TaskRunItem, TaskRunStateItem } from "../bindings";
import { chooseFolder, chooseOpenFile } from "../dialogs";
import { errorText, type Loaded } from "../dialogState";
import { useDialogs } from "../dialogStore";
import { useApp } from "../store";
import { connectionTag, listedConnection } from "../connectionKey";
import { startJob, type Job } from "../jobs";
import { ConfirmDelete } from "./ConfirmDelete";
import { formatBytes } from "./ServerObservation";

/** Lines of a running task's log kept on screen (the store keeps the whole run's). */
const LIVE_LINES = 200;

/** Where a run of the selected task stands, in this page only (ADR-060). */
type RunPhase =
  | { kind: "idle" }
  | { kind: "running"; job: Job; status: string; lines: string[] }
  | { kind: "ended"; state: TaskRunStateItem; text: string }
  | { kind: "failed"; text: string };

/** The dialog open over the page, if any. */
type Dialog =
  | { kind: "none" }
  | { kind: "confirm"; task: TaskItem; plan: TaskPlanItem; password: string | null }
  | { kind: "edit"; task: TaskItem }
  | { kind: "delete"; task: TaskItem };

/** Kinds whose confirmation loses data when run (its button is danger). */
const WRITES: TaskKindItem[] = ["import", "restore", "duckdbImport", "runFile"];

export function TasksPage() {
  const { t, i18n } = useTranslation("shell");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [tasks, setTasks] = useState<Loaded<TaskItem[]>>({ kind: "loading" });
  const [selected, setSelected] = useState<number | null>(null);
  const [dialog, setDialog] = useState<Dialog>({ kind: "none" });
  const [run, setRun] = useState<RunPhase>({ kind: "idle" });
  // Runs of the selected task, read again after each of its runs.
  const [runs, setRuns] = useState<Loaded<TaskRunItem[]>>({ kind: "idle" });
  const [reload, setReload] = useState(0);

  const load = useCallback(() => {
    setTasks((current) => (current.kind === "ready" ? current : { kind: "loading" }));
    call(commands.tasks()).then(
      (value) => {
        setTasks({ kind: "ready", value });
        setSelected((id) => (id !== null && value.some((task) => task.id === id) ? id : (value[0]?.id ?? null)));
      },
      (e: unknown) => setTasks({ kind: "failed", message: errorText(e) }),
    );
  }, []);
  useEffect(load, [load]);

  const list = tasks.kind === "ready" ? tasks.value : [];
  const task = list.find((item) => item.id === selected) ?? null;

  useEffect(() => {
    if (selected === null) {
      setRuns({ kind: "idle" });
      return;
    }
    let current = true;
    setRuns({ kind: "loading" });
    call(commands.taskRuns(selected)).then(
      (value) => current && setRuns({ kind: "ready", value }),
      (e: unknown) => current && setRuns({ kind: "failed", message: errorText(e) }),
    );
    return () => {
      current = false;
    };
  }, [selected, reload]);

  const folders = useMemo(() => {
    const groups = new Map<string, TaskItem[]>();
    for (const item of list) groups.set(item.folder, [...(groups.get(item.folder) ?? []), item]);
    return [...groups.entries()];
  }, [list]);

  /** Starts a run; the core asks back for a password or a yes, and nothing is recorded until
   *  the run starts with the answer. */
  async function start(item: TaskItem, password: string | null, confirmed: boolean) {
    const job = startJob();
    setRun({ kind: "running", job, status: t("tasks.running"), lines: [] });
    const events = new Channel<TaskEventItem>();
    events.onmessage = (event) =>
      setRun((phase) => {
        if (phase.kind !== "running" || phase.job !== job) return phase;
        if (event.kind === "log") return { ...phase, lines: [...phase.lines, event.line].slice(-LIVE_LINES) };
        return { ...phase, status: progressText(event, t, numbers, i18n.language) };
      });
    let outcome: TaskOutcomeItem;
    try {
      outcome = await call(commands.runTask(item.id, job.channel, password, confirmed, events));
    } catch (e) {
      setRun({ kind: "failed", text: t("tasks.couldNotRun", { reason: errorText(e) }) });
      return;
    }
    switch (outcome.kind) {
      case "ran":
        setRun({ kind: "ended", state: outcome.state, text: endedText(outcome.state, outcome.summary, outcome.message ?? null, t) });
        break;
      case "needsConfirmation":
        setRun({ kind: "idle" });
        setDialog({ kind: "confirm", task: item, plan: outcome.plan, password });
        return;
      case "passwordRequired": {
        setRun({ kind: "idle" });
        const name = connectionName(item);
        const answer = await useDialogs.getState().askPassword(name, false, null);
        if (answer) await start(item, answer.password, confirmed);
        return;
      }
      default: {
        const unexpected: never = outcome;
        return unexpected;
      }
    }
    load();
    setReload((n) => n + 1);
  }

  function connectionName(item: TaskItem): string {
    if (!item.connection) return "";
    const listed = listedConnection(useApp.getState().connections, { folder: item.connection.folder ?? null, id: item.connection.id });
    return listed?.name ?? item.connection.id;
  }

  const running = run.kind === "running";

  return (
    <section className="tasks-page" aria-labelledby="tasks-title">
      <header className="tasks-header">
        <h1 id="tasks-title">{t("tasks.title")}</h1>
        <p>{t("tasks.intro")}</p>
      </header>
      {tasks.kind === "failed" && (
        <div className="banner" role="alert">
          {t("tasks.loadFailed", { reason: tasks.message })}
          <button onClick={load}>{t("tasks.retry")}</button>
        </div>
      )}
      {tasks.kind === "loading" && <p className="empty">{t("tasks.loading")}</p>}
      {tasks.kind === "ready" && list.length === 0 && (
        <div className="empty tasks-empty">
          <p>{t("tasks.emptyTitle")}</p>
          <p className="muted">{t("tasks.emptyHint")}</p>
        </div>
      )}
      {list.length > 0 && (
        <div className="tasks-body">
          <nav className="tasks-list" aria-label={t("tasks.listLabel")}>
            {folders.map(([folder, items]) => (
              <div key={folder} className="tasks-folder">
                {folder && <h2 className="tasks-folder-name">{folder}</h2>}
                <ul>
                  {items.map((item) => (
                    <li key={item.id}>
                      <button
                        className={`tasks-row${item.id === selected ? " selected" : ""}`}
                        aria-current={item.id === selected ? "true" : undefined}
                        data-task={item.name}
                        onClick={() => {
                          if (!running) {
                            setSelected(item.id);
                            setRun({ kind: "idle" });
                          }
                        }}
                      >
                        <span className="tasks-row-name">{item.name}</span>
                        <span className="tasks-row-meta">
                          {item.kind ? t(`tasks.kinds.${item.kind}`) : t("tasks.unreadableShort")}
                          {item.lastRun ? ` · ${t(`tasks.states.${item.lastRun.state}`)}` : ""}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
              </div>
            ))}
          </nav>
          {task && (
            <TaskDetail
              key={task.id}
              task={task}
              connection={connectionName(task)}
              run={run}
              runs={runs}
              onRun={() => void start(task, null, false)}
              onCancel={() => run.kind === "running" && run.job.cancel()}
              onEdit={() => setDialog({ kind: "edit", task })}
              onDuplicate={() =>
                void call(commands.duplicateTask(task.id)).then(
                  (id) => {
                    setSelected(id);
                    load();
                  },
                  (e: unknown) => setRun({ kind: "failed", text: t("tasks.couldNotDuplicate", { reason: errorText(e) }) }),
                )
              }
              onDelete={() => setDialog({ kind: "delete", task })}
            />
          )}
        </div>
      )}
      {dialog.kind === "confirm" && (
        <ConfirmRun
          task={dialog.task}
          plan={dialog.plan}
          onCancel={() => setDialog({ kind: "none" })}
          onRun={() => {
            const { task: item, password } = dialog;
            setDialog({ kind: "none" });
            void start(item, password, true);
          }}
        />
      )}
      {dialog.kind === "edit" && (
        <EditTask
          task={dialog.task}
          onClose={(saved) => {
            setDialog({ kind: "none" });
            if (saved) load();
          }}
        />
      )}
      {dialog.kind === "delete" && (
        <ConfirmDelete
          title={t("tasks.deleteTitle")}
          action={t("tasks.deleteAction")}
          onCancel={() => setDialog({ kind: "none" })}
          onConfirm={async () => {
            await call(commands.deleteTask(dialog.task.id));
            setDialog({ kind: "none" });
            setSelected(null);
            load();
          }}
        >
          <p>{t("tasks.deleteBody", { name: dialog.task.name })}</p>
        </ConfirmDelete>
      )}
    </section>
  );
}

type T = ReturnType<typeof useTranslation>["t"];

function progressText(event: Exclude<TaskEventItem, { kind: "log" }>, t: T, numbers: Intl.NumberFormat, language: string): string {
  switch (event.kind) {
    case "rows":
      return t("tasks.progressRows", { rows: numbers.format(event.rows ?? 0), size: formatBytes(event.bytes ?? 0, language) });
    case "import":
      return t("tasks.progressImport", { read: numbers.format(event.read ?? 0), committed: numbers.format(event.committed ?? 0) });
    case "bytes":
      return t("tasks.progressBytes", { size: formatBytes(event.bytes ?? 0, language) });
    case "statements":
      return t("tasks.progressStatements", { done: numbers.format(event.done ?? 0), total: numbers.format(event.total ?? 0) });
    default: {
      const unexpected: never = event;
      return unexpected;
    }
  }
}

function endedText(state: TaskRunStateItem, summary: string, message: string | null, t: T): string {
  switch (state) {
    case "succeeded":
      return t("tasks.ended.succeeded", { summary });
    case "failed":
      return t("tasks.ended.failed", { reason: message ?? summary });
    case "cancelled":
      return summary && summary !== message ? t("tasks.ended.cancelledKept", { kept: summary }) : t("tasks.ended.cancelled");
    case "refused":
      return t("tasks.ended.refused", { reason: message ?? summary });
    case "unfinished":
      return t("tasks.states.unfinished");
    default: {
      const unexpected: never = state;
      return unexpected;
    }
  }
}

function TaskDetail(props: {
  task: TaskItem;
  connection: string;
  run: RunPhase;
  runs: Loaded<TaskRunItem[]>;
  onRun(): void;
  onCancel(): void;
  onEdit(): void;
  onDuplicate(): void;
  onDelete(): void;
}) {
  const { t, i18n } = useTranslation("shell");
  const { task, run } = props;
  const running = run.kind === "running";
  const [shownLog, setShownLog] = useState<{ run: number; log: Loaded<TaskLogItem> } | null>(null);
  const dates = useMemo(() => new Intl.DateTimeFormat(i18n.language, { dateStyle: "medium", timeStyle: "medium" }), [i18n.language]);

  function toggleLog(id: number) {
    if (shownLog?.run === id) {
      setShownLog(null);
      return;
    }
    setShownLog({ run: id, log: { kind: "loading" } });
    call(commands.taskRunLog(id)).then(
      (value) => setShownLog((shown) => (shown?.run === id ? { run: id, log: { kind: "ready", value } } : shown)),
      (e: unknown) => setShownLog((shown) => (shown?.run === id ? { run: id, log: { kind: "failed", message: errorText(e) } } : shown)),
    );
  }

  return (
    <div className="tasks-detail">
      <h2>{task.name}</h2>
      {task.unreadable ? (
        <p className="error" role="alert">
          {t("tasks.unreadable", { reason: task.unreadable })}
        </p>
      ) : (
        <dl className="tasks-facts">
          <dt>{t("tasks.kind")}</dt>
          <dd>{task.kind ? t(`tasks.kinds.${task.kind}`) : ""}</dd>
          <dt>{t("tasks.connection")}</dt>
          <dd>{props.connection}</dd>
          {task.path && (
            <>
              <dt>{t(task.kind && READS.includes(task.kind) ? "tasks.readsFrom" : "tasks.writesTo")}</dt>
              <dd className="tasks-path">{task.path}</dd>
            </>
          )}
          {task.sql && (
            <>
              <dt>{t("tasks.query")}</dt>
              <dd>
                <pre className="tasks-sql">{task.sql}</pre>
              </dd>
            </>
          )}
        </dl>
      )}
      <div className="tasks-actions">
        {running ? (
          <button onClick={props.onCancel}>{t("tasks.cancelRun")}</button>
        ) : (
          <button className="primary" disabled={!!task.unreadable} onClick={props.onRun}>
            {t("tasks.run")}
          </button>
        )}
        <button disabled={running || !!task.unreadable} onClick={props.onEdit}>
          {t("tasks.edit")}
        </button>
        <button disabled={running || !!task.unreadable} onClick={props.onDuplicate}>
          {t("tasks.duplicate")}
        </button>
        <button disabled={running} onClick={props.onDelete}>
          {t("tasks.delete")}
        </button>
      </div>
      <p className="muted small">{t("tasks.cliHint", { name: task.name })}</p>
      {run.kind === "running" && (
        <div className="tasks-live">
          <p role="status">{run.status}</p>
          {run.lines.length > 0 && <pre className="tasks-log">{run.lines.join("\n")}</pre>}
        </div>
      )}
      {run.kind === "ended" && (
        <p role="status" className={run.state === "succeeded" ? "tasks-result" : "error"}>
          {run.text}
        </p>
      )}
      {run.kind === "failed" && (
        <p role="alert" className="error">
          {run.text}
        </p>
      )}
      <h3 className="tasks-history-title">{t("tasks.history")}</h3>
      {props.runs.kind === "loading" && <p className="muted">{t("tasks.historyLoading")}</p>}
      {props.runs.kind === "failed" && (
        <p className="error" role="alert">
          {props.runs.message}
        </p>
      )}
      {props.runs.kind === "ready" && props.runs.value.length === 0 && <p className="muted">{t("tasks.historyEmpty")}</p>}
      {props.runs.kind === "ready" && props.runs.value.length > 0 && (
        <ul className="tasks-runs">
          {props.runs.value.map((item) => (
            <li key={item.id} className="tasks-run" data-origin={item.origin} data-state={item.state}>
              <div className="tasks-run-head">
                <span className="tasks-run-time">{dates.format(new Date(item.startedMs ?? 0))}</span>
                <span>{t(`tasks.origins.${item.origin}`)}</span>
                <span className={`tasks-run-state state-${item.state}`}>{t(`tasks.states.${item.state}`)}</span>
                <button className="ghost" aria-expanded={shownLog?.run === item.id} onClick={() => toggleLog(item.id ?? 0)}>
                  {shownLog?.run === item.id ? t("tasks.hideLog") : t("tasks.showLog")}
                </button>
              </div>
              {item.summary && <p className="tasks-run-summary">{item.summary}</p>}
              {shownLog?.run === item.id && (
                <>
                  {shownLog.log.kind === "loading" && <p className="muted">{t("tasks.logLoading")}</p>}
                  {shownLog.log.kind === "failed" && <p className="error">{shownLog.log.message}</p>}
                  {shownLog.log.kind === "ready" && (
                    <pre className="tasks-log">
                      {shownLog.log.value.log}
                      {(shownLog.log.value.omitted ?? 0) > 0 ? `\n${t("tasks.logOmitted", { count: shownLog.log.value.omitted ?? 0 })}` : ""}
                    </pre>
                  )}
                </>
              )}
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

/** Kinds whose path is read rather than written. */
const READS: TaskKindItem[] = ["import", "restore", "duckdbImport", "runFile"];

/** The confirmation of a run that asks first: what the core says it will run, and why it asks. */
function ConfirmRun({ task, plan, onCancel, onRun }: { task: TaskItem; plan: TaskPlanItem; onCancel(): void; onRun(): void }) {
  const { t } = useTranslation("shell");
  const destructive = WRITES.includes(plan.kind);
  const cancel = useRef<HTMLButtonElement>(null);
  const runButton = useRef<HTMLButtonElement>(null);
  useEffect(() => {
    // The safe choice has the focus when the run writes.
    (destructive ? cancel : runButton).current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") onCancel();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [destructive, onCancel]);
  return (
    <div className="modal-backdrop">
      <div className="modal tasks-confirm" role="dialog" aria-modal="true" aria-labelledby="tasks-confirm-title">
        <h2 id="tasks-confirm-title">{t("tasks.confirmTitle", { name: task.name })}</h2>
        <p>{t("tasks.confirmBody", { connection: plan.connection })}</p>
        <ul className="tasks-plan">
          {plan.steps.map((step, i) => (
            <li key={i}>
              <code>{step}</code>
            </li>
          ))}
        </ul>
        {plan.risks.length > 0 && (
          <>
            <p className="tasks-plan-label">{t("tasks.confirmRisks")}</p>
            <ul className="tasks-plan warning">
              {plan.risks.map((risk, i) => (
                <li key={i}>{risk}</li>
              ))}
            </ul>
          </>
        )}
        {plan.notes.map((note, i) => (
          <p key={i} className="muted small">
            {note}
          </p>
        ))}
        <p className="muted small">{t("tasks.confirmNothingYet")}</p>
        <div className="modal-actions">
          <button ref={cancel} onClick={onCancel}>
            {t("tasks.cancel")}
          </button>
          <button ref={runButton} className={destructive ? "danger" : ""} onClick={onRun}>
            {t("tasks.runAnyway")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Edit…: the name, the folder, the connection, the path and a query export's query. */
function EditTask({ task, onClose }: { task: TaskItem; onClose(saved: boolean): void }) {
  const { t } = useTranslation("shell");
  const connections = useApp((s) => s.connections);
  const [name, setName] = useState(task.name);
  const [folder, setFolder] = useState(task.folder);
  const original = task.connection ? connectionTag({ folder: task.connection.folder ?? null, id: task.connection.id }) : "";
  const [connection, setConnection] = useState(original);
  const [path, setPath] = useState(task.path ?? "");
  const [sql, setSql] = useState(task.sql ?? "");
  const [phase, setPhase] = useState<{ kind: "editing" } | { kind: "saving" } | { kind: "failed"; message: string }>({ kind: "editing" });
  const first = useRef<HTMLInputElement>(null);
  const saving = phase.kind === "saving";
  useEffect(() => {
    first.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !saving) onClose(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [saving, onClose]);
  const choices = connections.map((c) => ({ tag: connectionTag({ folder: c.folder ?? null, id: c.id }), key: { folder: c.folder ?? null, id: c.id }, name: c.name }));
  const listedOriginal = choices.some((c) => c.tag === original);
  const reads = task.kind !== null && task.kind !== undefined && READS.includes(task.kind);
  const folderPath = task.kind === "export" || task.kind === "dump" || task.kind === "embeddedDump" || task.kind === "duckdbImport";

  async function choosePath() {
    const chosen = folderPath ? await chooseFolder(t("tasks.choosePath"), path || null) : await chooseOpenFile(t("tasks.choosePath"), [], path || null);
    if (chosen) setPath(chosen);
  }

  async function save() {
    const chosen = choices.find((c) => c.tag === connection)?.key ?? (task.connection ? { folder: task.connection.folder ?? null, id: task.connection.id } : null);
    if (!chosen) return;
    setPhase({ kind: "saving" });
    try {
      await call(
        commands.editTask(task.id, {
          name,
          folder,
          connection: chosen,
          path: task.path ? path : null,
          sql: task.sql ? sql : null,
        }),
      );
      onClose(true);
    } catch (e) {
      setPhase({ kind: "failed", message: errorText(e) });
    }
  }

  return (
    <div className="modal-backdrop">
      <div className="modal tasks-edit" role="dialog" aria-modal="true" aria-labelledby="tasks-edit-title">
        <h2 id="tasks-edit-title">{t("tasks.editTitle")}</h2>
        <label className="field">
          {t("tasks.name")}
          <input ref={first} name="task-name" value={name} disabled={saving} onChange={(e) => setName(e.target.value)} />
        </label>
        <label className="field">
          {t("tasks.folder")}
          <input name="task-folder" value={folder} placeholder={t("tasks.folderPlaceholder")} disabled={saving} onChange={(e) => setFolder(e.target.value)} />
        </label>
        <label className="field">
          {t("tasks.connection")}
          <select value={connection} disabled={saving} onChange={(e) => setConnection(e.target.value)}>
            {!listedOriginal && task.connection && <option value={original}>{task.connection.id}</option>}
            {choices.map((c) => (
              <option key={c.tag} value={c.tag}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        {task.path && (
          <div className="field">
            <label htmlFor="task-path">{t(reads ? "tasks.readsFrom" : "tasks.writesTo")}</label>
            <div className="tasks-path-field">
              <input id="task-path" value={path} spellCheck={false} disabled={saving} onChange={(e) => setPath(e.target.value)} />
              <button disabled={saving} onClick={() => void choosePath()}>
                {t("tasks.choose")}
              </button>
            </div>
          </div>
        )}
        {task.sql !== null && task.sql !== undefined && (
          <label className="field">
            {t("tasks.query")}
            <textarea className="tasks-sql" value={sql} rows={6} spellCheck={false} disabled={saving} onChange={(e) => setSql(e.target.value)} />
          </label>
        )}
        <p className="muted small">{t("tasks.editHint")}</p>
        {phase.kind === "failed" && (
          <p className="error" role="alert">
            {t("tasks.couldNotSave", { reason: phase.message })}
          </p>
        )}
        <div className="modal-actions">
          <button disabled={saving} onClick={() => onClose(false)}>
            {t("tasks.cancel")}
          </button>
          <button className="primary" disabled={saving || !name.trim()} onClick={() => void save()}>
            {saving ? t("tasks.saving") : t("tasks.saveChanges")}
          </button>
        </div>
      </div>
    </div>
  );
}
