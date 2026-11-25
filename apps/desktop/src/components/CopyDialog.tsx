// The copy dialog (ADR-069): a table, copied to another connection (any engine). First where it
// goes: the connection, its schema, the new table's name (or a table that is there) and what to
// copy; then what the core wrote: each column's type there and what it loses, what is not
// copied, and the statements, which run only after they were shown. The core reads the source,
// maps the columns, writes and checks every statement and loads the rows: only the user's
// choices and the core's answers pass through here.

import { useEffect, useMemo, useRef, useState } from "react";
import { Channel } from "@tauri-apps/api/core";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands } from "../api";
import type { CopyHowItem, CopyPlanItem_Serialize, CopyRequestItem, IfExistsItem, ImportProgressItem, LossItem } from "../bindings";
import { connectTabFromExplorer, explorerDatabase, openSession, refreshExplorer } from "../connect";
import { connectionOfTag, connectionTag, keyOf, listedConnection, type ConnectionKey } from "../connectionKey";
import { explorerSessionOpen } from "../tabState";
import { errorText as message, loadedValue, type Loaded } from "../dialogState";
import { explorerSession } from "../explorerActions";
import { startJob } from "../jobs";
import { useApp, useDialogs } from "../store";
import { confirmShown, useJobPhase } from "../useDialogState";

/** The table the dialog copies. */
export interface CopyRequest {
  /** The source connection's identity (ADR-075). */
  connection: ConnectionKey;
  /** The source connection's explorer session: the copy opens its own from it. */
  session: string;
  schema: string;
  table: string;
}

export const useCopy = create<{ request: CopyRequest | null; open(request: CopyRequest): void; close(): void }>((set) => ({
  request: null,
  open: (request) => set({ request }),
  close: () => set({ request: null }),
}));

type Step = "target" | "review";
/** The rows read and committed, while the copy runs. */
type Progress = { read: number; committed: number };
/** A finished copy, and the table it loaded (to open it). */
type Done = { text: string; schema: string; table: string };
/** The target connection's explorer session, while it opens. */
type Target = { kind: "none" } | { kind: "connecting" } | { kind: "ready"; session: string } | { kind: "failed"; message: string };

export function CopyDialog() {
  const request = useCopy((s) => s.request);
  if (!request) return null;
  return <CopyForm key={`${request.session}-${request.schema}-${request.table}`} request={request} />;
}

function CopyForm({ request }: { request: CopyRequest }) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const close = useCopy((s) => s.close);
  const connections = useApp((s) => s.connections);
  const [engines, setEngines] = useState<string[]>([]);
  /** The target connection, as its `connectionTag` (the picker's value); empty: none. */
  const [targetTag, setTargetTag] = useState("");
  const targetKey = connectionOfTag(targetTag);
  const [target, setTarget] = useState<Target>({ kind: "none" });
  const [schemas, setSchemas] = useState<string[]>([]);
  const [schema, setSchema] = useState("");
  const [tables, setTables] = useState<string[]>([]);
  const [how, setHow] = useState<CopyHowItem>("structureAndData");
  const [tableName, setTableName] = useState(request.table);
  const [existing, setExisting] = useState("");
  const [ifExists, setIfExists] = useState<IfExistsItem>("fail");
  const [skip, setSkip] = useState<string[]>([]);
  const [types, setTypes] = useState<Record<string, string>>({});
  const [commitEvery, setCommitEvery] = useState("50000");
  const [step, setStep] = useState<Step>("target");
  const [planned, setPlanned] = useState<Loaded<CopyPlanItem_Serialize>>({ kind: "idle" });
  const plan = loadedValue(planned);
  const job = useJobPhase<Progress, Done, { text: string }>();
  const { phase, running } = job;
  const copyId = phase.kind === "running" ? phase.id : null;
  const sequence = useRef(0);
  const source = listedConnection(connections, request.connection) ?? null;

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // Only the topmost dialog closes: the confirmation opened over this one takes Escape.
      if (e.key === "Escape" && !running && !useDialogs.getState().risk) close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [running, close]);

  // The engines a copy can write to are the core's.
  useEffect(() => {
    void commands.copyTargets().then(setEngines).catch(() => setEngines([]));
  }, []);

  // The target connection's explorer session: connected as the explorer would (its password,
  // trust and changes asked as there) when it is not open yet.
  async function chooseTarget(tag: string) {
    setTargetTag(tag);
    const key = connectionOfTag(tag);
    setSchemas([]);
    setSchema("");
    setPlanned({ kind: "idle" });
    if (!key) {
      setTarget({ kind: "none" });
      return;
    }
    const session = explorerSession(key);
    setTarget({ kind: "connecting" });
    try {
      if (!explorerSessionOpen(useApp.getState().explorerConnected, key)) {
        const opened = await openSession(session, key);
        if (!opened) {
          setTarget({ kind: "none" });
          return;
        }
        useApp.getState().setExplorerConnected(key, true);
      }
      setTarget({ kind: "ready", session });
    } catch (e) {
      setTarget({ kind: "failed", message: message(e) });
    }
  }

  const targetSession = target.kind === "ready" ? target.session : null;

  useEffect(() => {
    if (!targetSession) return;
    let current = true;
    void call(commands.listSchemas(targetSession, null, false))
      .then((list) => {
        if (!current) return;
        const names = list.filter((s) => !s.system).map((s) => s.name);
        setSchemas(names);
        setSchema((now) => now || names[0] || "");
      })
      .catch((e: unknown) => current && setTarget({ kind: "failed", message: message(e) }));
    return () => {
      current = false;
    };
  }, [targetSession]);

  useEffect(() => {
    if (!targetSession || !schema) return;
    let current = true;
    void call(commands.listObjects(targetSession, schema, null, false))
      .then((objects) => current && setTables(objects.filter((o) => o.kind === "table" || o.kind === "partitioned_table").map((o) => o.name)))
      .catch(() => current && setTables([]));
    return () => {
      current = false;
    };
  }, [targetSession, schema]);

  function copyRequest(): CopyRequestItem | null {
    const table = how === "dataOnly" ? existing : tableName.trim();
    if (!targetSession || !schema || !table) return null;
    return {
      sourceSchema: request.schema,
      sourceTable: request.table,
      targetSchema: schema,
      targetTable: table,
      how,
      ifExists,
      skip,
      types: Object.entries(types).filter(([, v]) => v.trim()),
      commitEvery: Number(commitEvery || "0"),
    };
  }

  // The core writes the plan again whenever what it depends on changes.
  async function review() {
    const built = copyRequest();
    if (!built || !targetSession) return;
    const mine = ++sequence.current;
    setPlanned({ kind: "loading" });
    setStep("review");
    try {
      const value = await call(commands.copyPlan(request.session, targetSession, built));
      if (mine === sequence.current) setPlanned({ kind: "ready", value });
    } catch (e) {
      if (mine === sequence.current) setPlanned({ kind: "failed", message: message(e) });
    }
  }

  useEffect(() => {
    if (step === "review") void review();
    // Leaving a column out or typing its type: the plan again.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [skip, types]);

  async function start(confirmed: boolean) {
    const built = copyRequest();
    if (!built || !plan || !targetSession) return;
    const target = plan.target;
    if (!confirmed) {
      // The risks point into the statements, then the insert, then what runs after it.
      const asked = await confirmShown((targetKey && listedConnection(connections, targetKey)?.name) ?? "", {
        sql: target.insert,
        statements: [...target.statements, target.insert, ...target.finish],
        risks: target.risks,
      });
      if (asked === null) return;
      confirmed = asked;
    }
    const id = startJob();
    job.start(id, { read: 0, committed: 0 });
    const channel = new Channel<ImportProgressItem>();
    channel.onmessage = (p) => job.progress(id, { read: p.read ?? 0, committed: p.committed ?? 0 });
    try {
      const outcome = await call(commands.runCopy(request.session, targetSession, id.channel, built, confirmed, channel));
      switch (outcome.kind) {
        case "done":
        case "stoppedAtLimit": {
          const done = outcome.result;
          const seconds = new Intl.NumberFormat(i18n.language, { maximumFractionDigits: 1 }).format((done.millis ?? 0) / 1000);
          job.done({
            text: t("copy.done", { count: done.rows ?? 0, formattedCount: numbers.format(done.rows ?? 0), table: done.table, seconds }),
            schema: done.schema,
            table: done.table,
          });
          if (done.createdTable && targetKey) refreshExplorer(targetKey);
          break;
        }
        case "needsConfirmation":
          job.reset();
          await start(false);
          return;
        case "cancelled":
          job.failed({ text: t("copy.cancelled", { count: outcome.kept ?? 0, formattedCount: numbers.format(outcome.kept ?? 0) }) });
          break;
        case "failed":
        case "notARead":
          job.failed({ text: t("copy.failed", { reason: outcome.message }) });
          break;
        case "passwordRequired":
          job.failed({ text: t("copy.failed", { reason: t("backup.passwordRequired") }) });
          break;
        default: {
          const unexpected: never = outcome;
          return unexpected;
        }
      }
    } catch (e) {
      job.failed({ text: t("copy.failed", { reason: message(e) }) });
    }
  }

  function openTable(done: { schema: string; table: string }) {
    if (!targetSession) return;
    if (!targetKey) return;
    const tab = useApp.getState().addTableTab(targetKey, done.schema, done.table);
    connectTabFromExplorer(tab, targetSession, explorerDatabase(targetKey, done.schema));
    close();
  }

  const targetChoices = connections.filter((c) => engines.includes(c.engine));
  const title = t("copy.title", { table: `${request.schema}.${request.table}`, connection: source?.name ?? "" });
  const blocked = plan ? plan.columns.filter((c) => c.blocked && !c.skipped) : [];
  const lossText = (loss: LossItem): string => {
    switch (loss.kind) {
      case "decimalNarrowed":
      case "decimalBounded":
        return t(`copy.loss.${loss.kind}`, { precision: loss.precision, scale: loss.scale });
      case "zoneToUtc":
      case "uuidAsText":
      case "jsonAsText":
      case "timeAsText":
      case "asText":
      case "decimalApproximate":
      case "lengthDropped":
      case "binaryNotYet":
        return t(`copy.loss.${loss.kind}`);
      default: {
        const unexpected: never = loss;
        return unexpected;
      }
    }
  };

  return (
    <div className="modal-backdrop">
      <div className="modal import-dialog" role="dialog" aria-modal="true" aria-label={title}>
        <h2>{title}</h2>
        <ol className="import-steps" aria-label={t("copy.steps")}>
          {(["target", "review"] as const).map((s) => (
            <li key={s} aria-current={step === s ? "step" : undefined}>
              {t(`copy.step.${s}`)}
            </li>
          ))}
        </ol>

        {step === "target" && (
          <>
            <fieldset className="import-target">
              <legend>{t("copy.target")}</legend>
              <label className="field">
                {t("copy.connection")}
                <select value={targetTag} disabled={target.kind === "connecting"} onChange={(e) => void chooseTarget(e.target.value)}>
                  <option value="">{t("copy.chooseConnection")}</option>
                  {targetChoices.map((c) => (
                    <option key={connectionTag(keyOf(c))} value={connectionTag(keyOf(c))}>
                      {c.name}
                    </option>
                  ))}
                </select>
              </label>
              {target.kind === "connecting" && <p className="muted" role="status">{t("copy.connecting")}</p>}
              {target.kind === "failed" && (
                <p className="error" role="alert">
                  {target.message}
                </p>
              )}
              {targetSession && (
                <>
                  <label className="field">
                    {t("copy.schema")}
                    <select value={schema} onChange={(e) => setSchema(e.target.value)}>
                      {schemas.map((s) => (
                        <option key={s} value={s}>
                          {s}
                        </option>
                      ))}
                    </select>
                  </label>
                  <label className="field">
                    {t("copy.what")}
                    <select value={how} onChange={(e) => setHow(e.target.value as CopyHowItem)}>
                      <option value="structureAndData">{t("copy.how.structureAndData")}</option>
                      <option value="structureOnly">{t("copy.how.structureOnly")}</option>
                      <option value="dataOnly">{t("copy.how.dataOnly")}</option>
                    </select>
                  </label>
                  {how === "dataOnly" ? (
                    <label className="field">
                      {t("copy.existingTable")}
                      <select value={existing} onChange={(e) => setExisting(e.target.value)}>
                        <option value="">{t("copy.chooseTable")}</option>
                        {tables.map((name) => (
                          <option key={name} value={name}>
                            {name}
                          </option>
                        ))}
                      </select>
                    </label>
                  ) : (
                    <>
                      <label className="field">
                        {t("copy.tableName")}
                        <input type="text" value={tableName} onChange={(e) => setTableName(e.target.value)} />
                      </label>
                      <label className="field">
                        {t("copy.ifExists")}
                        <select value={ifExists} onChange={(e) => setIfExists(e.target.value as IfExistsItem)}>
                          <option value="fail">{t("import.ifExistsFail")}</option>
                          <option value="replace">{t("import.ifExistsReplace")}</option>
                          <option value="append">{t("import.ifExistsAppend")}</option>
                        </select>
                      </label>
                    </>
                  )}
                  {how !== "structureOnly" && (
                    <label className="field">
                      {t("import.commitEvery")}
                      <input type="text" inputMode="numeric" value={commitEvery} onChange={(e) => setCommitEvery(e.target.value.replace(/[^0-9]/g, ""))} />
                    </label>
                  )}
                </>
              )}
            </fieldset>
            <div className="modal-actions">
              <button onClick={close}>{t("import.close")}</button>
              <button className="primary" disabled={!copyRequest()} onClick={() => void review()}>
                {t("import.next")}
              </button>
            </div>
          </>
        )}

        {step === "review" && (
          <>
            {planned.kind === "loading" && <p className="muted" role="status">{t("copy.planning")}</p>}
            {planned.kind === "failed" && (
              <p className="error" role="alert">
                {t("copy.couldNotPlan", { reason: planned.message })}
              </p>
            )}
            {plan && (
              <>
                <div className="import-mapping" role="region" aria-label={t("copy.columns")}>
                  <table>
                    <thead>
                      <tr>
                        <th>{t("copy.copyColumn")}</th>
                        <th>{t("import.sourceColumn")}</th>
                        <th>{t("copy.sourceType")}</th>
                        <th>{t("copy.targetType")}</th>
                        <th>{t("copy.losses")}</th>
                      </tr>
                    </thead>
                    <tbody>
                      {plan.columns.map((c) => (
                        <tr key={c.name}>
                          <td>
                            <input
                              type="checkbox"
                              aria-label={t("copy.include", { name: c.name })}
                              checked={!c.skipped}
                              disabled={running}
                              onChange={(e) => setSkip((now) => (e.target.checked ? now.filter((n) => n !== c.name) : [...now, c.name]))}
                            />
                          </td>
                          <td className="import-column-name">{c.name}</td>
                          <td className="muted code">{c.sourceType}</td>
                          <td>
                            {how === "dataOnly" ? (
                              <span className="muted">{t("copy.existingColumn")}</span>
                            ) : (
                              <input
                                type="text"
                                className="code"
                                aria-label={t("import.typeFor", { name: c.name })}
                                defaultValue={types[c.name] ?? c.targetType}
                                disabled={c.skipped || running}
                                onBlur={(e) => {
                                  const value = e.target.value.trim();
                                  if (value && value !== c.targetType) setTypes((now) => ({ ...now, [c.name]: value }));
                                }}
                              />
                            )}
                          </td>
                          <td>
                            {c.blocked && !c.skipped ? (
                              <span className="error">{t(`copy.blocked.${c.blocked}`)}</span>
                            ) : (
                              <span className="muted">{c.losses.map(lossText).join(" ")}</span>
                            )}
                          </td>
                        </tr>
                      ))}
                    </tbody>
                  </table>
                </div>
                <p className="muted small">
                  {t("copy.notCopied", {
                    indexes: plan.notCopied.indexes,
                    foreignKeys: plan.notCopied.foreignKeys,
                    constraints: plan.notCopied.constraints,
                    defaults: plan.notCopied.defaults,
                  })}
                  {plan.notCopied.primaryKey ? ` ${t("copy.primaryKeyNotCopied")}` : ""}
                </p>
                <pre className="import-sql">{[...plan.target.statements, plan.target.insert].join(";\n\n")}</pre>
                {plan.target.finish.length > 0 && <pre className="import-sql">{plan.target.finish.join(";\n\n")}</pre>}
                <p className="muted small">{t("import.bound")}</p>
              </>
            )}
            {phase.kind === "running" && (
              <p role="status" className="import-progress">
                {t("import.progress", { count: phase.progress.read, formattedCount: numbers.format(phase.progress.read), committed: numbers.format(phase.progress.committed) })}
              </p>
            )}
            {phase.kind === "done" && (
              <p role="status" className="import-result">
                {phase.text}
              </p>
            )}
            {phase.kind === "failed" && (
              <p role="alert" className="error import-result">
                {phase.text}
              </p>
            )}
            <div className="modal-actions">
              {phase.kind === "running" ? (
                <button disabled={copyId === null} onClick={() => copyId?.cancel()}>
                  {t("copy.cancel")}
                </button>
              ) : phase.kind === "done" ? (
                <>
                  <button onClick={close}>{t("import.close")}</button>
                  <button className="primary" onClick={() => openTable(phase)}>
                    {t("import.openTable")}
                  </button>
                </>
              ) : (
                <>
                  <button
                    onClick={() => {
                      job.reset();
                      setStep("target");
                    }}
                  >
                    {t("import.back")}
                  </button>
                  <button className="primary" disabled={!plan || blocked.length > 0} onClick={() => void start(false)}>
                    {phase.kind === "failed" ? t("copy.again") : t("copy.start")}
                  </button>
                </>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
