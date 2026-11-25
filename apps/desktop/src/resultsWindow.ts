// A tab's results in a window of their own (SDB-30, ADR-083).
//
// The rows never move: they stay in the core's store, and the results window asks for windows
// of the same results by id, as the tab's own grid does, so nothing is run or fetched again.
// What moves between the two pages is what the tab's page already keeps about its run (the
// results' metadata, its messages, whether it runs: never a row), sent by the tab's window on
// every change, and the results window's requests back (a sort, a filter, the next page...),
// which the tab's window carries out with its own handlers, exactly as its own panel would. The
// tab's window stays the only owner of its run: the results window only shows it and asks.
//
// Both directions are events sent to one window (`emitTo` its label) and heard on that window
// only (`getCurrentWebviewWindow().listen`).

import { emitTo } from "@tauri-apps/api/event";
import { getCurrentWebviewWindow } from "@tauri-apps/api/webviewWindow";
import { call, commands, type ColumnFilterItem, type ConnectionItem } from "./api";
import i18n from "./i18n";
import type { FilterMode, RunState, SortState } from "./store";
import { windowLabel } from "./windowContext";

/** The tab's window to its results window: what the tab's panel would show. */
export const MIRROR_EVENT = "spelaion://results-mirror";
/** The results window to the tab's window: a request about the tab's results. */
export const REQUEST_EVENT = "spelaion://results-request";
/** The tab's window's answer to a request that returns something. */
export const REPLY_EVENT = "spelaion://results-reply";

/** What the tab's results panel shows, sent to its results window. Metadata only. */
export interface ResultsMirror {
  tab: string;
  /** The tab's name, as its tab shows it. */
  name: string;
  connectionId: string | null;
  run: RunState | null;
  running: boolean;
  connectFailed: boolean;
  runStatus: string;
  cancelRequested: boolean;
  refreshDisabled: boolean;
  restoringView: boolean;
}

/** What the results window asks of the tab: the same callbacks its own panel calls. */
export type ResultsRequest =
  | { kind: "hello" }
  | { kind: "select"; id: number | null }
  | { kind: "sorted"; result: number; sort: SortState[] }
  | { kind: "filtered"; result: number; filter: string | undefined; rows: number; total: number }
  | { kind: "columnFiltered"; result: number; columnFilters: ColumnFilterItem[]; rows: number; total: number; filterMode?: FilterMode }
  | { kind: "refresh"; result: number; row: number }
  | { kind: "loadMore"; result: number }
  | { kind: "runAgain"; result: number }
  | { kind: "countQuery"; result: number }
  | { kind: "queryPageSize"; size: number }
  | { kind: "goToRow"; result: number; row: number }
  | { kind: "loadRows"; result: number; until: number | null }
  | { kind: "cancelLoad" }
  | { kind: "cancelRun" }
  | { kind: "querySort"; result: number; sort: SortState[] };

export interface RequestEnvelope {
  /** The tab's session key. */
  tab: string;
  /** The window that asks, to answer it. */
  from: string;
  /** Tells one answer from another. */
  seq: number;
  request: ResultsRequest;
}

export interface ReplyEnvelope {
  seq: number;
  value: unknown;
}

/** Shows the tab's results in a window of their own (or brings it forward). Its label, or the
 *  core's refusal. */
export async function detachResults(tab: string, name: string): Promise<string> {
  return call(commands.openResultsWindow(tab, i18n.t("app:detach.windowTitle", { name })));
}

/** Closes the tab's results window: the tab shows its results again when the core says so. */
export async function reattachResults(tab: string): Promise<void> {
  await call(commands.closeResultsWindow(tab));
}

/** Opens `connection` in a window of its own (SDB-30). */
export async function openConnectionWindow(connection: ConnectionItem): Promise<string> {
  return call(commands.openConnectionWindow({ folder: connection.folder, id: connection.id }, i18n.t("app:detach.connectionTitle", { name: connection.name })));
}

/** The tab's window: sends what its panel shows to the results window `label`. */
export function sendMirror(label: string, mirror: ResultsMirror): Promise<void> {
  return emitTo(label, MIRROR_EVENT, mirror);
}

/** The tab's window: hears the requests for `tab`, carries each out with `handle` and answers
 *  the ones that return something. Returns the way to stop hearing them. */
export function serveRequests(tab: string, handle: (request: ResultsRequest) => unknown): () => void {
  const stop = getCurrentWebviewWindow().listen<RequestEnvelope>(REQUEST_EVENT, (event) => {
    const envelope = event.payload;
    if (envelope.tab !== tab) return;
    void Promise.resolve(handle(envelope.request))
      .catch(() => null)
      .then((value) => emitTo(envelope.from, REPLY_EVENT, { seq: envelope.seq, value: value ?? null } satisfies ReplyEnvelope));
  });
  return () => void stop.then((unlisten) => unlisten());
}

/** The results window: asks the tab's window `owner`, and waits for its answer. */
export function ownerClient(owner: string, tab: string) {
  let seq = 0;
  const waiting = new Map<number, (value: unknown) => void>();
  const stop = getCurrentWebviewWindow().listen<ReplyEnvelope>(REPLY_EVENT, (event) => {
    const resolve = waiting.get(event.payload.seq);
    if (!resolve) return;
    waiting.delete(event.payload.seq);
    resolve(event.payload.value);
  });
  const ask = (request: ResultsRequest): Promise<unknown> => {
    seq += 1;
    const envelope: RequestEnvelope = { tab, from: windowLabel(), seq, request };
    return new Promise((resolve) => {
      waiting.set(envelope.seq, resolve);
      void emitTo(owner, REQUEST_EVENT, envelope).catch(() => {
        waiting.delete(envelope.seq);
        resolve(null);
      });
    });
  };
  return {
    ask,
    close: () => void stop.then((unlisten) => unlisten()),
  };
}
