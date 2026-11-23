// The outline of a script (SDB-382): one row per statement, as the core's splitter reads it
// for the tab's engine (the splitter that runs the script). A row puts the cursor on its
// statement. The core is asked again a moment after the text stops changing, never per key.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { OutlineItem } from "../bindings";
import { byteToIndex } from "../text";
import { Icon } from "./Icons";

/** How long the text rests before the core is asked for the outline again. */
const OUTLINE_DELAY_MS = 300;

export function ScriptOutline({ sql, engine, onShow, onClose }: {
  sql: string;
  /** The tab's engine; with no connection the script is read as plain SQL. */
  engine: string | null;
  /** Puts the editor's cursor at this index (JS) of the text. */
  onShow(index: number): void;
  onClose(): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  // The outline with the text it was read from: its positions are that text's.
  const [outline, setOutline] = useState<{ sql: string; items: OutlineItem } | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const asked = useRef(0);
  useEffect(() => {
    const mine = ++asked.current;
    // The first reading comes at once; later ones wait for the typing to stop.
    const timer = window.setTimeout(() => {
      call(commands.scriptOutline(engine ?? "postgres", sql))
        .then((items) => {
          if (mine !== asked.current) return;
          setOutline({ sql, items });
          setFailure(null);
        })
        .catch((e: unknown) => {
          if (mine === asked.current) setFailure(e instanceof Error ? e.message : String(e));
        });
    }, outline === null ? 0 : OUTLINE_DELAY_MS);
    return () => window.clearTimeout(timer);
    // `outline` only chooses the first delay.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sql, engine]);

  const entries = outline?.items.entries ?? [];
  const more = (outline?.items.total ?? 0) - entries.length;
  return (
    <aside className="script-outline" aria-label={t("outline.title")}>
      <div className="script-outline-head">
        <h3>{t("outline.title")}</h3>
        <button type="button" className="ghost small" aria-label={t("outline.close")} title={t("outline.close")} onClick={onClose}><Icon name="x" /></button>
      </div>
      {failure ? (
        <div className="script-outline-note error" role="status">{t("outline.failed", { reason: failure })}</div>
      ) : outline && entries.length === 0 ? (
        <div className="script-outline-note">{t("outline.empty")}</div>
      ) : (
        <ol className="script-outline-list">
          {entries.map((entry) => (
            <li key={`${entry.span.start}:${entry.span.end}`}>
              <button
                type="button"
                className="script-outline-entry"
                aria-label={t("outline.entry", { line: entry.line, summary: entry.summary })}
                title={entry.summary}
                // The positions belong to the text the outline was read from: while the editor
                // holds a newer one, the row waits for the next reading.
                disabled={outline?.sql !== sql}
                onClick={() => outline && onShow(byteToIndex(outline.sql, entry.span.start))}
              >
                <span className="script-outline-line" aria-hidden="true">{entry.line}</span>
                <span className="script-outline-summary">{entry.summary}</span>
              </button>
            </li>
          ))}
        </ol>
      )}
      {more > 0 && <div className="script-outline-note">{t("outline.more", { count: more, formattedCount: numbers.format(more) })}</div>}
    </aside>
  );
}
