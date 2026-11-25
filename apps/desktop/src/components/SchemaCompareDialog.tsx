// "Compare schemas…" (ADR-070): the source is the schema whose menu opened it; the target is a
// schema of a connection connected in the explorer (the same one, or another of the same engine).
// The core reads both sides, each in a session of its own opened for reads only, and compares
// them; the dialog shows its report and asks it for the script, which opens in a new editor tab
// on the target's connection, never run from here (ADR-026, ADR-053).

import { useEffect, useMemo, useState } from "react";
import type { TFunction } from "i18next";
import { useTranslation } from "react-i18next";
import { Channel } from "@tauri-apps/api/core";
import { call, commands } from "../api";
import type {
  CompareProgressItem,
  ComparedPartItem,
  McpDiffLineItem,
  ObjectComparisonItem_Serialize,
  PartComparisonItem_Serialize,
  SchemaComparisonItem_Serialize,
} from "../bindings";
import { errorText } from "../dialogState";
import { explorerSession, openSqlTab } from "../explorerActions";
import { startJob } from "../jobs";
import { useSchemaCompare } from "../schemaCompare";
import { useApp } from "../store";
import { explorerSessionOpen } from "../tabState";
import { connectionOfTag, connectionTag, keyOf, listedConnection } from "../connectionKey";
import type { CompareSource } from "../schemaCompare";
import { useJobPhase } from "../useDialogState";

type Report = SchemaComparisonItem_Serialize;
type Done = { report: Report } | { cancelled: true };

export function SchemaCompareDialog() {
  const source = useSchemaCompare((s) => s.source);
  const close = useSchemaCompare((s) => s.close);
  if (!source) return null;
  return <CompareDialog key={JSON.stringify([connectionTag(source.connection), source.schema])} source={source} onClose={close} />;
}

function CompareDialog({ source, onClose }: { source: CompareSource; onClose(): void }) {
  const { t } = useTranslation("shell");
  const connections = useApp((s) => s.connections);
  const connected = useApp((s) => s.explorerConnected);
  const sourceConnection = listedConnection(connections, source.connection);
  /** The source's and the target's identities as their tags: the picker's values and the
   *  effects' dependencies (ADR-075). */
  const sourceTag = connectionTag(source.connection);
  // Connections the comparison can read now: connected in the explorer, of the source's engine.
  const targets = useMemo(
    () => connections.filter((c) => c.sql && c.engine === sourceConnection?.engine && explorerSessionOpen(connected, keyOf(c))),
    [connections, connected, sourceConnection?.engine],
  );
  const [target, setTarget] = useState(sourceTag);
  const targetKey = connectionOfTag(target) ?? source.connection;
  const [schemas, setSchemas] = useState<string[] | null>(null);
  const [targetSchema, setTargetSchema] = useState("");
  const [unavailable, setUnavailable] = useState<string | null>(null);
  const [selected, setSelected] = useState(0);
  const [drops, setDrops] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const job = useJobPhase<CompareProgressItem | null, Done, { message: string }>();
  const report = job.phase.kind === "done" && "report" in job.phase ? job.phase.report : null;

  // Whether both sides can be compared, from the core: an engine without structure forms says why.
  useEffect(() => {
    let current = true;
    void (async () => {
      for (const tag of new Set([sourceTag, target])) {
        const reason = await call(commands.schemaCompareSupport(explorerSession(connectionOfTag(tag) ?? source.connection))).catch(errorText);
        if (!current) return;
        if (reason) return setUnavailable(reason);
      }
      if (current) setUnavailable(null);
    })();
    return () => { current = false; };
  }, [sourceTag, target]);

  // The target connection's schemas, read from the server; another schema than the source's
  // first when the target is the same connection.
  useEffect(() => {
    let current = true;
    setSchemas(null);
    call(commands.listSchemas(explorerSession(targetKey), null, false))
      .then((list) => {
        if (!current) return;
        const names = list.filter((s) => !s.system).map((s) => s.name);
        setSchemas(names);
        const other = target === sourceTag ? names.find((n) => n !== source.schema) : undefined;
        setTargetSchema(other ?? names.find((n) => n === source.schema) ?? names[0] ?? "");
      })
      .catch((e: unknown) => {
        if (!current) return;
        setSchemas([]);
        setError(errorText(e));
      });
    return () => { current = false; };
  }, [target, sourceTag, source.schema]);

  // The core keeps a comparison until the dialog lets it go.
  useEffect(() => {
    if (!report) return;
    const id = report.comparison;
    return () => void call(commands.forgetSchemaComparison(id)).catch(() => undefined);
  }, [report]);

  async function compare() {
    setError(null);
    setSelected(0);
    const id = startJob();
    job.start(id, null);
    const progress = new Channel<CompareProgressItem>();
    progress.onmessage = (event) => job.progress(id, event);
    try {
      const outcome = await call(
        commands.compareSchemas(explorerSession(source.connection), source.schema, explorerSession(targetKey), targetSchema, id.channel, progress),
      );
      switch (outcome.kind) {
        case "done":
        case "stoppedAtLimit":
          return job.done({ report: outcome.result });
        case "cancelled":
          return job.done({ cancelled: true });
        case "failed":
        case "notARead":
          return job.failed({ message: outcome.message });
        case "needsConfirmation":
        case "passwordRequired":
          return job.failed({ message: outcome.kind });
        default: {
          const never: never = outcome;
          return never;
        }
      }
    } catch (e) {
      job.failed({ message: errorText(e) });
    }
  }

  async function openScript() {
    if (!report) return;
    setError(null);
    try {
      const script = await call(commands.schemaCompareScript(report.comparison, drops));
      openSqlTab(targetKey, explorerSession(targetKey), targetSchema, script.sql);
      onClose();
    } catch (e) {
      setError(t("compare.scriptFailed", { detail: errorText(e) }));
    }
  }

  const running = job.phase.kind === "running";
  const title = t("compare.title");
  const object = report?.objects[selected] ?? null;
  return (
    <div className="modal-backdrop">
      <form
        className="modal schema-compare-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(e) => {
          e.preventDefault();
          if (report) void openScript();
          else void compare();
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape" && !running) { e.stopPropagation(); onClose(); }
        }}
      >
        <h2>{title}</h2>
        <p className="muted small">{t("compare.source", { connection: sourceConnection?.name ?? source.connection.id, schema: source.schema })}</p>
        <div className="form-pair">
          <label className="field grow">
            <span>{t("compare.targetConnection")}</span>
            <select name="targetConnection" value={target} disabled={running} onChange={(e) => { setTarget(e.target.value); job.reset(); }}>
              {targets.map((c) => (
                <option key={connectionTag(keyOf(c))} value={connectionTag(keyOf(c))}>{c.name}</option>
              ))}
            </select>
          </label>
          <label className="field grow">
            <span>{t("compare.targetSchema")}</span>
            <select name="targetSchema" value={targetSchema} disabled={running || !schemas} onChange={(e) => { setTargetSchema(e.target.value); job.reset(); }}>
              {(schemas ?? []).map((name) => (
                <option key={name} value={name}>{name}</option>
              ))}
            </select>
          </label>
        </div>
        {unavailable && <p className="warn small" role="status">{t("compare.unavailable", { reason: unavailable })}</p>}

        {job.phase.kind === "running" && (
          <p className="small" role="status">
            {job.phase.progress
              ? t("compare.reading", {
                  side: t(`compare.sides.${job.phase.progress.side}`),
                  done: job.phase.progress.done,
                  total: job.phase.progress.total,
                })
              : t("compare.starting")}
          </p>
        )}
        {job.phase.kind === "done" && "cancelled" in job.phase && <p className="small" role="status">{t("compare.cancelled")}</p>}
        {job.phase.kind === "failed" && (
          <p className="error small" role="alert">{t("compare.failed", { detail: job.phase.message })}</p>
        )}

        {report && (
          <>
            <p className="small" role="status">
              {report.objects.length === 0
                ? t("compare.same", { count: report.same })
                : t("compare.summary", { count: report.objects.length, same: report.same })}
            </p>
            {report.objects.length > 0 && (
              <div className="schema-compare-report">
                <ul className="schema-compare-objects" role="listbox" aria-label={t("compare.objects")}>
                  {report.objects.map((o, i) => (
                    <li key={`${o.kind}\u0000${o.name}\u0000${o.detail ?? ""}`}>
                      <button
                        type="button"
                        role="option"
                        aria-selected={i === selected}
                        className={i === selected ? "selected" : undefined}
                        onClick={() => setSelected(i)}
                      >
                        <span className="mono">{o.detail ? `${o.name}(${o.detail})` : o.name}</span>
                        <span className="muted small">
                          {t(`compare.kinds.${o.kind}`)} · {t(`compare.status.${o.status.kind}`)}
                        </span>
                      </button>
                    </li>
                  ))}
                </ul>
                <div className="schema-compare-details" aria-live="polite">
                  {object && <ObjectDetails object={object} sourceSchema={report.sourceSchema} />}
                </div>
              </div>
            )}
            <p className="muted small">
              {t("compare.notCompared", { items: report.notCompared.map((n) => t(`compare.notComparedItems.${n}`)).join(", ") })}
            </p>
            {report.objects.length > 0 && (
              <>
                <label className="check">
                  <input type="checkbox" name="drops" checked={drops} onChange={(e) => setDrops(e.target.checked)} />
                  <span>{t("compare.drops")}</span>
                </label>
                <p className="muted small">{t("compare.opensInTab")}</p>
              </>
            )}
          </>
        )}
        {!report && !running && targets.length < 2 && schemas?.length === 1 && <p className="muted small">{t("compare.noTarget")}</p>}
        {error && <p className="error small" role="alert">{error}</p>}

        <div className="modal-actions">
          {running && (
            <button type="button" onClick={() => job.phase.kind === "running" && job.phase.id?.cancel()}>
              {t("compare.cancel")}
            </button>
          )}
          {report && (
            <button type="button" onClick={() => void compare()}>
              {t("compare.again")}
            </button>
          )}
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={running}>
            {t("cancel")}
          </button>
          {report && report.objects.length > 0 ? (
            <button type="submit" className="primary">
              {t("compare.openScript")}
            </button>
          ) : (
            !report && (
              <button type="submit" className="primary" disabled={running || !!unavailable || !targetSchema}>
                {t("compare.run")}
              </button>
            )
          )}
        </div>
      </form>
    </div>
  );
}

function partLabel(t: TFunction<"shell">, part: ComparedPartItem): string {
  switch (part.kind) {
    case "object":
    case "primaryKey":
      return t(`compare.parts.${part.kind}`);
    case "column":
    case "constraint":
    case "index":
    case "foreignKey":
      return t(`compare.parts.${part.kind}`, { name: part.name });
    default: {
      const never: never = part;
      return never;
    }
  }
}

function PartRow({ part }: { part: PartComparisonItem_Serialize }) {
  const { t } = useTranslation("shell");
  const label = partLabel(t, part.part);
  switch (part.status.kind) {
    case "onlyInSource":
    case "onlyInTarget":
      return (
        <tr>
          <th scope="row">{label}</th>
          <td colSpan={3}>
            {t(`compare.status.${part.status.kind}`)}: <span className="mono">{part.status.definition}</span>
          </td>
        </tr>
      );
    case "changed":
      return (
        <>
          {part.status.changes.map((change) => (
            <tr key={change.property}>
              <th scope="row">{label}</th>
              <td>{t(`compare.properties.${change.property}`)}</td>
              <td className="mono">{change.property === "source" ? "…" : change.source ?? t("compare.none")}</td>
              <td className="mono">{change.property === "source" ? "…" : change.target ?? t("compare.none")}</td>
            </tr>
          ))}
        </>
      );
    default: {
      const never: never = part.status;
      return never;
    }
  }
}

function DiffLines({ lines }: { lines: McpDiffLineItem[] }) {
  const { t } = useTranslation("shell");
  return (
    <pre className="mcp-diff schema-compare-diff" aria-label={t("compare.properties.source")}>
      {lines.map((line, i) => {
        switch (line.kind) {
          case "same":
            return <div key={i}>{`  ${line.text}`}</div>;
          case "added":
            return <div key={i} className="diff-added">{`+ ${line.text}`}</div>;
          case "removed":
            return <div key={i} className="diff-removed">{`- ${line.text}`}</div>;
          case "gap":
            return <div key={i} className="diff-gap">{t("compare.gap", { count: line.skipped })}</div>;
          default: {
            const never: never = line.kind;
            return never;
          }
        }
      })}
    </pre>
  );
}

function ObjectDetails({ object, sourceSchema }: { object: ObjectComparisonItem_Serialize; sourceSchema: string }) {
  const { t } = useTranslation("shell");
  const status = object.status;
  switch (status.kind) {
    case "onlyInSource":
    case "onlyInTarget":
      return <p className="small">{t(`compare.status.${status.kind}`)}</p>;
    case "changed":
      return (
        <>
          <table className="schema-compare-properties">
            <thead>
              <tr>
                <th scope="col">{t("compare.part")}</th>
                <th scope="col">{t("compare.property")}</th>
                <th scope="col">{t("compare.sourceHeader")}</th>
                <th scope="col">{t("compare.targetHeader")}</th>
              </tr>
            </thead>
            <tbody>
              {status.parts.map((part, i) => (
                <PartRow key={i} part={part} />
              ))}
            </tbody>
          </table>
          {status.sourceDiff.length > 0 && <DiffLines lines={status.sourceDiff} />}
          {status.namesSourceSchema && <p className="warn small">{t("compare.namesSourceSchema", { schema: sourceSchema })}</p>}
        </>
      );
    default: {
      const never: never = status;
      return never;
    }
  }
}
