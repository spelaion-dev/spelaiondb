// Filter box of a result: the core keeps the rows where some cell contains the text, and
// the grid, copies and exports follow.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { ResultMeta } from "../store";

const DELAY_MS = 250;

export function ResultFilter(props: {
  result: ResultMeta;
  onFiltered(filter: string | undefined, rows: number, total: number): void;
}) {
  const { result } = props;
  const { t } = useTranslation("editor");
  const [text, setText] = useState(result.filter ?? "");
  const [error, setError] = useState<string | null>(null);
  // Only the answer to the last request counts.
  const latest = useRef(0);
  const callbacks = useRef(props);
  callbacks.current = props;

  useEffect(() => {
    const wanted = text.trim();
    if (wanted === (result.filter ?? "")) return;
    const request = ++latest.current;
    const timer = setTimeout(() => {
      call(commands.filterResult(result.id, wanted || null))
        .then((outcome) => {
          if (request !== latest.current) return;
          setError(null);
          callbacks.current.onFiltered(wanted || undefined, outcome.rows, outcome.total);
        })
        .catch((e: unknown) => request === latest.current && setError(e instanceof Error ? e.message : String(e)));
    }, DELAY_MS);
    return () => clearTimeout(timer);
  }, [text, result.id, result.filter]);

  return (
    <input
      type="search"
      className={`result-filter ${error ? "invalid" : ""}`}
      placeholder={t("results.filter")}
      aria-label={t("results.filter")}
      title={error ?? (result.complete ? t("results.filterHint") : t("results.filterWhenComplete"))}
      disabled={!result.complete}
      value={text}
      onChange={(e) => setText(e.target.value)}
      onKeyDown={(e) => {
        if (e.key === "Escape" && text) {
          e.stopPropagation();
          setText("");
        }
      }}
    />
  );
}
