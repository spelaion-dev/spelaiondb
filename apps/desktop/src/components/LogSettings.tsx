// Settings › Logs (SDB-31): the app log, newest first, filtered by level, and exported to a file
// for a report. The core reads, scrubs and filters it (`app_log.rs`): the page only asks again
// when the level changes or on Refresh. The log holds what went wrong and when, never SQL, data
// or passwords.

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { LogLevelItem, LogPageItem } from "../bindings";
import { errorText, type Loaded } from "../dialogState";
import { chooseSavePath } from "../dialogs";

const LEVELS: LogLevelItem[] = ["info", "warning", "error"];

/** What the last export did: only the page has it. */
type Exported =
  | { kind: "none" }
  | { kind: "exporting" }
  | { kind: "done"; path: string; count: number }
  | { kind: "failed"; detail: string };

export function LogSettings() {
  const { t } = useTranslation("settings");
  const [level, setLevel] = useState<LogLevelItem>("info");
  const [page, setPage] = useState<Loaded<LogPageItem>>({ kind: "idle" });
  const [exported, setExported] = useState<Exported>({ kind: "none" });

  const load = useCallback(async (shown: LogLevelItem) => {
    setPage({ kind: "loading" });
    try {
      setPage({ kind: "ready", value: await call(commands.readLog(shown)) });
    } catch (e) {
      setPage({ kind: "failed", message: errorText(e) });
    }
  }, []);

  useEffect(() => {
    void load(level);
  }, [level, load]);

  const exportLog = async () => {
    const path = await chooseSavePath({
      title: t("logs.exportTitle"),
      defaultPath: "spelaion.log",
      filters: [{ name: t("logs.fileType"), extensions: ["log", "txt"] }],
    });
    if (!path) return;
    setExported({ kind: "exporting" });
    try {
      const count = await call(commands.exportLog(level, path));
      setExported({ kind: "done", path, count });
    } catch (e) {
      setExported({ kind: "failed", detail: errorText(e) });
    }
  };

  return (
    <div className="log-settings">
      <p>{t("logs.hint")}</p>
      <div className="log-toolbar">
        <label className="field">
          {t("logs.show")}
          <select value={level} onChange={(event) => setLevel(event.target.value as LogLevelItem)}>
            {LEVELS.map((option) => (
              <option key={option} value={option}>{t(`logs.levels.${option}`)}</option>
            ))}
          </select>
        </label>
        <button disabled={page.kind === "loading"} onClick={() => void load(level)}>{t("logs.refresh")}</button>
        <button disabled={exported.kind === "exporting"} onClick={() => void exportLog()}>{t("logs.export")}</button>
      </div>
      <ExportState exported={exported} />
      <LogPage page={page} level={level} />
    </div>
  );
}

function ExportState({ exported }: { exported: Exported }) {
  const { t } = useTranslation("settings");
  switch (exported.kind) {
    case "none":
      return null;
    case "exporting":
      return <p className="muted small" role="status">{t("logs.exporting")}</p>;
    case "done":
      return <p className="small log-path" role="status">{t("logs.exported", { total: exported.count, path: exported.path })}</p>;
    case "failed":
      return <p className="error small" role="alert">{t("logs.exportFailed", { detail: exported.detail })}</p>;
    default: {
      const never: never = exported;
      return never;
    }
  }
}

function LogPage({ page, level }: { page: Loaded<LogPageItem>; level: LogLevelItem }) {
  const { t } = useTranslation("settings");
  switch (page.kind) {
    case "idle":
    case "loading":
      return <p className="muted small" role="status">{t("logs.loading")}</p>;
    case "failed":
      return <p className="error small" role="alert">{t("logs.loadFailed", { detail: page.message })}</p>;
    case "ready": {
      const { value } = page;
      const total = value.errors + value.warnings + value.infos;
      return (
        <>
          <p className="muted small log-path">
            {t("logs.counts", { errors: value.errors, warnings: value.warnings, infos: value.infos, path: value.path })}
          </p>
          {value.entries.length === 0 ? (
            <p className="empty muted">{total === 0 ? t("logs.empty") : t("logs.noneAtLevel", { level: t(`logs.levels.${level}`) })}</p>
          ) : (
            <ol className="log-entries" aria-label={t("logs.entries")}>
              {value.entries.map((entry, index) => (
                <li key={`${entry.time}-${index}`} data-level={entry.level}>
                  <span className={`log-level log-level-${entry.level}`}>{t(`logs.level.${entry.level}`)}</span>
                  <time className="log-time" dateTime={entry.time || undefined}>{entry.time}</time>
                  <pre className="log-message">{entry.message}</pre>
                </li>
              ))}
            </ol>
          )}
        </>
      );
    }
    default: {
      const never: never = page;
      return never;
    }
  }
}
