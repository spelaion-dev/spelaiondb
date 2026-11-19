// What an editor tab says about its session's run (SDB-370). The core publishes the state
// (`runChanged`); this only words it, so `scripts/run-state.test.mjs` can check it without
// React. Nothing here decides whether a run is cancelled or paused.

import type { RunStateItem } from "./bindings";
import type { Translation } from "./store";

/** The status line while the tab's run goes: running, "Paused at statement 3", cancelling. */
export function runStatus(state: RunStateItem | undefined): Translation {
  if (!state) return { key: "results.running" };
  switch (state.kind) {
    // Idle comes with the run's last event: until the page has drawn it, it is still running.
    case "idle":
    case "running":
      return { key: "results.running" };
    case "paused":
      return { key: "results.paused", values: { index: state.statement + 1 } };
    case "cancelling":
      return { key: "results.cancelling" };
    default:
      return unknownState(state);
  }
}

/** A cancel is on its way: Cancel is not offered again until the run ends. */
export function isCancelling(state: RunStateItem | undefined): boolean {
  return state?.kind === "cancelling";
}

function unknownState(state: never): never {
  throw new Error(`unknown run state: ${JSON.stringify(state)}`);
}
