// How a run's events change the result tabs of an editor tab (src/components/EditorTab.tsx),
// kept free of React so `scripts/run-results.test.mjs` can check it. Which results a run
// replaces, a result's name and its pin are the core's (SDB-378): this only draws what the
// core's events say.

interface EndingResult {
  statement: number;
  /** The run that produced it: an editor tab keeps results of several runs. */
  run?: number;
  rows: number;
  complete: boolean;
  paging?: { pageSize: number; hasMore: boolean };
}

/** The results of statement `index` of run `run` once it has ended: complete, as the core's
 *  stored results are once their statement is done or has failed (a failed statement keeps the
 *  rows that arrived, and nothing more comes; a cancel or a timeout fails it too). A page that
 *  filled up may have another after it, only when its statement finished. Results kept from
 *  other runs are not touched. */
export function endStatementResults<R extends EndingResult>(results: R[], index: number, outcome: "finished" | "failed", run?: number): R[] {
  return results.map((result) =>
    result.statement === index && result.run === run
      ? {
          ...result,
          complete: true,
          ...(result.paging ? { paging: { ...result.paging, hasMore: outcome === "finished" && result.rows === result.paging.pageSize } } : {}),
        }
      : result,
  );
}

interface TabResult {
  id: number;
  run?: number;
}

interface TabResults<R extends TabResult> {
  results: R[];
  activeResult: number | null;
}

/** The result tabs without the ones the core closed. An active tab that was closed gives way
 *  to its neighbour: the one after it, else the one before, else the messages. */
export function withoutResults<R extends TabResult, S extends TabResults<R>>(state: S, closed: readonly number[]): S {
  if (!closed.some((id) => state.results.some((result) => result.id === id))) return state;
  const results = state.results.filter((result) => !closed.includes(result.id));
  let activeResult = state.activeResult;
  if (activeResult !== null && closed.includes(activeResult)) {
    const at = state.results.findIndex((result) => result.id === activeResult);
    const next = state.results.slice(at + 1).find((result) => !closed.includes(result.id));
    const previous = state.results.slice(0, at).reverse().find((result) => !closed.includes(result.id));
    activeResult = (next ?? previous)?.id ?? null;
  }
  return { ...state, results, activeResult };
}

/** The result tabs with a result that just started, where the core put it: in the place of the
 *  result it replaces (a re-run), else after the results its run already has there, else last.
 *  The first result of a run is shown; so is one whose active tab it took the place of. */
export function withStartedResult<R extends TabResult, S extends TabResults<R>>(state: S, started: R, replaces: number | null, closed: readonly number[]): S {
  const firstOfRun = !state.results.some((result) => result.run === started.run);
  const replacedAt = replaces === null ? -1 : state.results.findIndex((result) => result.id === replaces);
  let results: R[];
  if (replacedAt >= 0) {
    results = state.results.map((result, at) => (at === replacedAt ? started : result));
  } else {
    const lastOfRun = state.results.reduce((last, result, at) => (result.run === started.run ? at : last), -1);
    // A run's results stay together only when they are not already at the end (a re-run in
    // place that returns several results).
    const at = lastOfRun >= 0 ? lastOfRun + 1 : state.results.length;
    results = [...state.results.slice(0, at), started, ...state.results.slice(at)];
  }
  const activeResult = firstOfRun || state.activeResult === replaces ? started.id : state.activeResult;
  const next = withoutResults({ ...state, results, activeResult }, closed.filter((id) => id !== started.id));
  return next;
}

/** A run that ended without any result of its own shows its messages (rows affected, or its
 *  error), as it did when a run replaced every result. */
export function afterRun<R extends TabResult, S extends TabResults<R>>(state: S, run: number): S {
  return state.results.some((result) => result.run === run) ? state : { ...state, activeResult: null };
}
