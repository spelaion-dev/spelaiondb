// Filter box of a result: the core keeps the rows where some cell contains the text, and
// the grid, copies and exports follow.

import { useEffect, useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { ResultMeta } from "../store";

const DELAY_MS = 250;

export function ResultFilter(props: {
  result: ResultMeta;
  /** Pending edits refer to rows by position: the view must not change meanwhile. */
  locked?: boolean;
  /** Table data filters apply to the current page; paged query filters apply to loaded rows. */
  scope?: "page" | "loaded";
  errorId: string;
  onError(error: string | null): void;
  onPending(pending: boolean): void;
  onFiltered(filter: string | undefined, rows: number, total: number): void;
}) {
  const { result } = props;
  const { t } = useTranslation("editor");
  const [text, setText] = useState(result.filter ?? "");
  const [error, setError] = useState<string | null>(null);
  const scopeId = useId();
  // Only the answer to the last request counts.
  const latest = useRef(0);
  const callbacks = useRef(props);
  callbacks.current = props;

  useEffect(() => {
    const wanted = text.trim();
    const request = ++latest.current;
    if (wanted === (result.filter ?? "")) {
      setError(null);
      callbacks.current.onError(null);
      callbacks.current.onPending(false);
      return;
    }
    const timer = setTimeout(() => {
      call(commands.filterResult(result.id, wanted || null))
        .then((outcome) => {
          if (request !== latest.current) return;
          setError(null);
          callbacks.current.onError(null);
          callbacks.current.onFiltered(wanted || undefined, outcome.rows, outcome.total);
          callbacks.current.onPending(false);
        })
        .catch((e: unknown) => { if (request === latest.current) { const message = e instanceof Error ? e.message : String(e); setError(message); callbacks.current.onError(message); callbacks.current.onPending(false); } });
    }, DELAY_MS);
    return () => {
      clearTimeout(timer);
      if (latest.current === request) latest.current += 1;
    };
  }, [text, result.id, result.filter]);

  return (
    <div className="result-filter-control">
      <input
        type="search"
        className={`result-filter ${error ? "invalid" : ""}`}
        placeholder={t("results.filter")}
        aria-label={t("results.filter")}
        aria-invalid={!!error}
        aria-describedby={[props.scope ? scopeId : null, error ? props.errorId : null].filter(Boolean).join(" ") || undefined}
        title={error ?? (!result.complete ? t("results.filterWhenComplete") : props.locked ? t("results.filterLocked") : props.scope === "page" ? t("results.filterPageHint") : props.scope === "loaded" ? t("queryPaging.filterLoaded") : t("results.filterHint"))}
        disabled={!result.complete || props.locked}
        value={text}
        onChange={(e) => { callbacks.current.onPending(true); setText(e.target.value); }}
        onKeyDown={(e) => {
          if (e.key === "Escape" && text) {
            e.stopPropagation();
            callbacks.current.onPending(true);
            setText("");
          }
        }}
      />
      {props.scope && <span className="result-filter-scope" title={t(props.scope === "page" ? "results.filterPageHint" : "queryPaging.filterLoaded")}>
        {t(props.scope === "page" ? "results.filterPageOnly" : "queryPaging.loadedOnly")}
        <span id={scopeId} className="sr-only">{t(props.scope === "page" ? "results.filterPageHint" : "queryPaging.filterLoaded")}</span>
      </span>}
    </div>
  );
}
