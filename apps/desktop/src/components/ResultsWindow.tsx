// A tab's results in a window of their own (SDB-30, ADR-083): the same results panel as in
// the tab, over the same results in the core. It holds no run of its own: what it shows comes
// from the tab's window (`resultsWindow.ts`), and what it is asked to do goes back there, so
// the tab stays the one owner of its run, its session and its results.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { call, commands, type ColumnFilterItem } from "../api";
import type { WindowContextItem } from "../bindings";
import { useModalFocus } from "../modalFocus";
import { MIRROR_EVENT, ownerClient, type ResultsMirror } from "../resultsWindow";
import type { FilterMode, SortState } from "../store";
import { Dialogs } from "./Dialogs";
import { Results } from "./EditorTab";
import { ExportDialog } from "./ExportDialog";
import { Icon } from "./Icons";
import { ScreenBoundary } from "./ScreenBoundary";
import { refreshConnections } from "./Sidebar";

type ResultsContext = Extract<WindowContextItem, { kind: "results" }>;

/** How often the window asks the tab again until the tab first answers. */
const HELLO_RETRY_MS = 400;

export function ResultsWindow({ context }: { context: ResultsContext }) {
  useModalFocus();
  const { t } = useTranslation("editor");
  const [mirror, setMirror] = useState<ResultsMirror | null>(null);
  /** Whether the tab's first word arrived, for the asking loop (which a render does not see). */
  const shown = useRef(false);
  shown.current = mirror !== null;
  const owner = useMemo(() => ownerClient(context.owner, context.tab), [context.owner, context.tab]);

  useEffect(() => {
    // The connections' names and engines, as the panel shows them; nothing is connected.
    void refreshConnections().catch(() => undefined);
    const heard = getCurrentWebviewWindow().listen<ResultsMirror>(MIRROR_EVENT, (event) => {
      if (event.payload.tab === context.tab) setMirror(event.payload);
    });
    // Once it listens, it asks the tab for what its panel shows; again until it hears, since
    // the tab may start listening only after this window first asks (and its first word may
    // come before this window listens).
    let asking: ReturnType<typeof setInterval> | undefined;
    void heard.then(() => {
      void owner.ask({ kind: "hello" });
      asking = setInterval(() => {
        if (shown.current) clearInterval(asking);
        else void owner.ask({ kind: "hello" });
      }, HELLO_RETRY_MS);
    });
    return () => {
      clearInterval(asking);
      void heard.then((unlisten) => unlisten());
      owner.close();
    };
  }, [context.tab, owner]);

  // "Re-attach results": this window closes, and the core tells the tab to show them again.
  const reattach = () => void call(commands.closeWindow()).catch(() => undefined);

  return (
    <div className="results-window">
      <div className="results-window-bar">
        <Icon name="detach" />
        <h1 className="results-window-title">{t("detach.heading", { name: mirror?.name ?? "" })}</h1>
        <button title={t("detach.reattachHint")} onClick={reattach}>{t("detach.reattach")}</button>
      </div>
      <ScreenBoundary screen="results window" className="results-window-body">
        {mirror === null ? (
          <div className="empty" role="status">{t("detach.waiting")}</div>
        ) : (
          <Results
            run={mirror.run}
            running={mirror.running}
            connectFailed={mirror.connectFailed}
            runStatus={mirror.runStatus}
            cancelRequested={mirror.cancelRequested}
            onCancelRun={() => void owner.ask({ kind: "cancelRun" })}
            session={context.tab}
            connectionId={mirror.connectionId}
            table={null}
            // Row edits stay in the tab, whose pending edits the core keeps with its session.
            editingState="readOnly"
            pendingEdits={false}
            refreshDisabled={mirror.refreshDisabled}
            restoringView={mirror.restoringView}
            rowOffset={0}
            onSelect={(id: number | null) => void owner.ask({ kind: "select", id })}
            onSorted={(result: number, sort: SortState[]) => void owner.ask({ kind: "sorted", result, sort })}
            onFiltered={(result: number, filter: string | undefined, rows: number, total: number) =>
              void owner.ask({ kind: "filtered", result, filter, rows, total })}
            onColumnFiltered={(result: number, columnFilters: ColumnFilterItem[], rows: number, total: number, filterMode?: FilterMode) =>
              void owner.ask({ kind: "columnFiltered", result, columnFilters, rows, total, filterMode })}
            onQuerySort={async (result: number, sort: SortState[]) => (await owner.ask({ kind: "querySort", result, sort })) === true}
            onRefresh={(result: number, row: number) => void owner.ask({ kind: "refresh", result, row })}
            onLoadMore={async (result: number) => {
              const loaded = await owner.ask({ kind: "loadMore", result });
              return typeof loaded === "number" ? loaded : 0;
            }}
            onRunAgain={(result: number) => void owner.ask({ kind: "runAgain", result })}
            onCountQuery={(result: number) => void owner.ask({ kind: "countQuery", result })}
            onQueryPageSize={(size: number) => void owner.ask({ kind: "queryPageSize", size })}
            onGoToRow={(result: number, row: number) => void owner.ask({ kind: "goToRow", result, row })}
            onLoadRows={async (result: number, until: number | null) => {
              const held = await owner.ask({ kind: "loadRows", result, until });
              return typeof held === "number" ? held : null;
            }}
            onCancelLoad={() => void owner.ask({ kind: "cancelLoad" })}
          />
        )}
      </ScreenBoundary>
      <ExportDialog />
      <Dialogs />
    </div>
  );
}
