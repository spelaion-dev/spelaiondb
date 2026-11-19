// The transposed view of a result (SDB-386): the current row, or the selected rows, shown as
// columns, one row per column name. The core makes it a result of its own (ADR-010), so this
// grid asks for its windows like any other and copies, the cell viewer, Ctrl+F and the calc
// panel work on it as they do on the rows. It is read-only, and freed when it closes.

import { useEffect, useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { ResultMeta } from "../store";
import { ResultGrid } from "./ResultGrid";

export function TransposedView(props: {
  source: ResultMeta;
  /** Positions in the source's current view. */
  rows: number[];
  /** The number the grid gives the view's first row (a table data page's offset plus 1). */
  firstNumber: number;
  session: string;
  onClose(): void;
}) {
  const { t, i18n } = useTranslation("editor");
  const numbers = useMemo(() => new Intl.NumberFormat(i18n.language), [i18n.language]);
  const [view, setView] = useState<ResultMeta | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [viewerOpen, setViewerOpen] = useState(false);
  const [viewerWidth, setViewerWidth] = useState(420);
  const { source, rows, firstNumber } = props;

  useEffect(() => {
    let made: number | null = null;
    let gone = false;
    call(commands.transposeRows(source.id, rows, firstNumber, t("transpose.column"), t("transpose.type")))
      .then((item) => {
        if (gone) {
          void call(commands.releaseResult(item.result)).catch(() => undefined);
          return;
        }
        made = item.result;
        setView({ id: item.result, statement: source.statement, run: source.run, columns: item.columns, rows: item.rows, end: { kind: "whole" }, pinned: false });
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)));
    return () => {
      gone = true;
      if (made !== null) void call(commands.releaseResult(made)).catch(() => undefined);
    };
    // The view is made once: a change of the source's rows mounts a new one (its key).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  const shown = [...new Set(rows)].sort((a, b) => a - b).map((row) => numbers.format(firstNumber + row));
  return (
    <div className="transposed-view">
      <div className="transposed-caption small muted" role="status">
        {t("transpose.caption", { count: shown.length, rows: shown.join(", ") })}
      </div>
      {error && (
        <div className="banner" role="alert">
          {t("transpose.failed", { detail: error })}
          <button type="button" className="ghost small" onClick={props.onClose}>{t("transpose.hide")}</button>
        </div>
      )}
      {view && (
        <div className="result-view">
          <ResultGrid
            result={view}
            session={props.session}
            onSorted={(sort) => setView((current) => (current ? { ...current, sort } : current))}
            onColumnFiltered={(columnFilters, rowsLeft, total, filterMode) =>
              setView((current) => (current ? { ...current, columnFilters, filterMode, rows: rowsLeft, total } : current))}
            viewerOpen={viewerOpen}
            onViewerOpen={setViewerOpen}
            viewerWidth={viewerWidth}
            onViewerWidth={setViewerWidth}
            pagingLocked
            onLoadMore={() => Promise.resolve(0)}
            onCountQuery={() => undefined}
            onQueryPageSize={() => undefined}
          />
        </div>
      )}
    </div>
  );
}
