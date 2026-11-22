// The search panel's data search as one state (ADR-060): its tables read so far and one phase,
// running, cancelling, ended with the core's outcome, or failed. Never flags that could disagree
// (a search both running and ended, or cancelling after it ended). Tested by
// scripts/search-run.test.mjs.
//
// Every change is applied to the run as it is when React applies it, never to the state a click
// handler saw when the panel last rendered (SDB-422): a search that has ended in between (its
// last table read, its summary shown) stays ended. Writing the rendered snapshot back put a
// finished search back to "running, cancelling", and nothing would ever end it again: the panel
// stayed at "Cancelling…" with Cancel disabled.

import type { DataSearchEventItem, DataSearchOutcome, TableStatusItem } from "./api";

/** A table the search has read, and what it found there. */
export interface SearchedTable {
  schema: string;
  table: string;
  status: TableStatusItem;
}

export type SearchPhase =
  /** `current` is the table being read, once the core has said which. */
  | { kind: "running"; current: { index: number; schema: string; table: string } | null }
  | { kind: "cancelling" }
  | { kind: "ended"; outcome: DataSearchOutcome }
  | { kind: "failed"; message: string };

/** `I` is whatever tells two runs apart: the panel uses the search's job (SDB-369). */
export interface SearchRun<I = number> {
  id: I;
  connectionId: string;
  /** The tables it reads. */
  total: number;
  tables: SearchedTable[];
  phase: SearchPhase;
}

/** A search that starts: nothing read yet. */
export function startedRun<I>(id: I, connectionId: string, total: number): SearchRun<I> {
  return { id, connectionId, total, tables: [], phase: { kind: "running", current: null } };
}

/** Whether the search is still going (running, or cancelling until the core says it stopped). */
export function searching<I>(run: SearchRun<I> | null): boolean {
  if (!run) return false;
  switch (run.phase.kind) {
    case "running":
    case "cancelling":
      return true;
    case "ended":
    case "failed":
      return false;
    default: {
      const unexpected: never = run.phase;
      return unexpected;
    }
  }
}

/** What the core told of search `id` while it runs. */
export function withEvent<I>(run: SearchRun<I> | null, id: I, event: DataSearchEventItem): SearchRun<I> | null {
  if (!run || run.id !== id) return run;
  switch (event.kind) {
    case "started":
      return {
        ...run,
        total: event.total,
        phase: run.phase.kind === "running" ? { kind: "running", current: { index: event.index, schema: event.schema, table: event.table } } : run.phase,
      };
    case "finished":
      return { ...run, tables: [...run.tables, { schema: event.schema, table: event.table, status: event.status }] };
    default: {
      const unexpected: never = event;
      return unexpected;
    }
  }
}

/** Search `id` ended, as the core says (a cancel asked earlier ends here too). */
export function ended<I>(run: SearchRun<I> | null, id: I, outcome: DataSearchOutcome): SearchRun<I> | null {
  return run && run.id === id ? { ...run, phase: { kind: "ended", outcome } } : run;
}

/** Search `id` could not run. */
export function failed<I>(run: SearchRun<I> | null, id: I, message: string): SearchRun<I> | null {
  return run && run.id === id ? { ...run, phase: { kind: "failed", message } } : run;
}

/** `run` with its cancel asked, if it is search `id` and still running; otherwise unchanged. */
export function markCancelling<I>(run: SearchRun<I> | null, id: I): SearchRun<I> | null {
  if (!run || run.id !== id || run.phase.kind !== "running") return run;
  return { ...run, phase: { kind: "cancelling" } };
}
