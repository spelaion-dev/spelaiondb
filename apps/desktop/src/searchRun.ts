// How the search panel's data search changes state when Cancel is clicked (SDB-422).
//
// The cancel is applied to the state as it is when React applies it, never to the state the
// click handler saw when the panel last rendered: a search that has ended in between (its last
// table read, its summary shown) stays ended. Writing the rendered snapshot back put a finished
// search back to "running, cancelling", and nothing would ever end it again: the panel stayed at
// "Cancelling…" with Cancel disabled.

/** What the cancel needs of a search run. */
export interface CancellableRun {
  id: number;
  running: boolean;
  cancelling: boolean;
}

/** `run` with its cancel asked, if it is search `id` and still running; otherwise unchanged. */
export function markCancelling<T extends CancellableRun>(run: T | null, id: number): T | null {
  if (!run || run.id !== id || !run.running || run.cancelling) return run;
  return { ...run, cancelling: true };
}
