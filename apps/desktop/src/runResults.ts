// How a run's events end its results (src/components/EditorTab.tsx), kept free of React so
// `scripts/run-results.test.mjs` can check it.

interface EndingResult {
  statement: number;
  rows: number;
  complete: boolean;
  paging?: { pageSize: number; hasMore: boolean };
}

/** The results of statement `index` once it has ended: complete, as the core's stored results
 *  are once their statement is done or has failed (a failed statement keeps the rows that
 *  arrived, and nothing more comes; a cancel or a timeout fails it too). A page that filled
 *  up may have another after it, only when its statement finished. */
export function endStatementResults<R extends EndingResult>(results: R[], index: number, outcome: "finished" | "failed"): R[] {
  return results.map((result) =>
    result.statement === index
      ? {
          ...result,
          complete: true,
          ...(result.paging ? { paging: { ...result.paging, hasMore: outcome === "finished" && result.rows === result.paging.pageSize } } : {}),
        }
      : result,
  );
}
