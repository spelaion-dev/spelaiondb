// The hooks of the dialogs' shared states (`dialogState.ts`, ADR-060): "preview, then apply" and
// a job's phase. A new dialog uses these instead of its own preview, error, busy and id flags.

import { useCallback, useEffect, useMemo, useReducer, useRef } from "react";
import type { RiskKind } from "./bindings";
import type { Job } from "./jobs";
import {
  errorText,
  initialPreviewApply,
  jobReducer,
  lastPreview,
  previewApplyReducer,
  previewFor,
  type JobPhase,
  type Loaded,
  type PreviewApplyState,
} from "./dialogState";
import { useDialogs } from "./store";

/** What a preview says may need the user's confirmation (the core's `AlterationPreviewItem`). */
export interface ShownScript {
  sql: string;
  statements: string[];
  risks: { statement: number; risk: RiskKind }[];
}

/**
 * Asks about the risks the core found in the script shown, before Apply runs it. True when the
 * user confirmed them, false when there were none, null when the user kept it (nothing runs).
 * The core checks again: this only asks.
 */
export async function confirmShown(connectionName: string, shown: ShownScript): Promise<boolean | null> {
  if (shown.risks.length === 0) return false;
  const statements = shown.risks.map((r) => ({ sql: shown.statements[r.statement] ?? shown.sql, risk: r.risk }));
  return (await useDialogs.getState().askRisks(connectionName, statements)) ? true : null;
}

export interface PreviewThenApply<P> {
  /** The preview of the form as it is now. */
  preview: Loaded<P>;
  /** The last preview answered, whichever form it was for. */
  last: Loaded<P>;
  apply: PreviewApplyState<P>["apply"];
  /** Apply is running: the dialog's buttons are disabled and Escape does nothing. */
  busy: boolean;
  /**
   * Runs Apply. `run` resolves with a note when the dialog stays open after it (empty when it
   * closes); a failure is kept as `failed(detail)`, with the detail as the core gave it.
   */
  run(run: () => Promise<string | void>, failed?: (detail: string) => string): Promise<void>;
  /** The note of the last apply no longer applies to what the form shows. */
  noteRead(): void;
}

/**
 * A dialog whose change the core writes as a script: the script follows the form (`key`, the
 * form as text; null when there is nothing to preview), asked `delay` ms after the last change,
 * and an answer to an earlier form is never taken for this one's.
 */
export function usePreviewThenApply<P>(
  session: string,
  key: string | null,
  delay: number,
  read: (key: string) => Promise<P>,
): PreviewThenApply<P> {
  const [state, dispatch] = useReducer(previewApplyReducer<P>, undefined, initialPreviewApply<P>);
  const reader = useRef(read);
  reader.current = read;

  useEffect(() => {
    if (key === null) {
      dispatch({ type: "cleared" });
      return;
    }
    let current = true;
    const timer = window.setTimeout(() => {
      reader.current(key).then(
        (value) => { if (current) dispatch({ type: "answered", key, value }); },
        (e: unknown) => { if (current) dispatch({ type: "refused", key, message: errorText(e) }); },
      );
    }, delay);
    return () => {
      current = false;
      window.clearTimeout(timer);
    };
  }, [session, key, delay]);

  const run = useCallback(async (apply: () => Promise<string | void>, failed: (detail: string) => string = (detail) => detail) => {
    dispatch({ type: "applying" });
    try {
      const note = await apply();
      dispatch({ type: "applied", note: note ?? "" });
    } catch (e) {
      dispatch({ type: "applyFailed", message: failed(errorText(e)) });
    }
  }, []);
  const noteRead = useCallback(() => dispatch({ type: "noteRead" }), []);

  return {
    preview: previewFor(state, key),
    last: lastPreview(state),
    apply: state.apply,
    busy: state.apply.kind === "applying",
    run,
    noteRead,
  };
}

export interface JobControls<P, D, F> {
  phase: JobPhase<P, D, F, Job>;
  running: boolean;
  /** A run starts as `job` (`startJob()`, SDB-369; null when it cannot be cancelled). */
  start(job: Job | null, progress: P): void;
  progress(job: Job | null, progress: P): void;
  done(done: D): void;
  failed(failed: F): void;
  reset(): void;
}

/** The phase of a job the dialog runs (an export, an import, a backup, a file run). */
export function useJobPhase<P, D, F>(): JobControls<P, D, F> {
  const [phase, dispatch] = useReducer(jobReducer<P, D, F, Job>, { kind: "idle" } as JobPhase<P, D, F, Job>);
  const controls = useMemo(
    () => ({
      start: (id: Job | null, progress: P) => dispatch({ type: "started", id, started: Date.now(), progress }),
      progress: (id: Job | null, progress: P) => dispatch({ type: "progressed", id, progress }),
      done: (done: D) => dispatch({ type: "done", done }),
      failed: (failed: F) => dispatch({ type: "failed", failed }),
      reset: () => dispatch({ type: "reset" }),
    }),
    [],
  );
  return { phase, running: phase.kind === "running", ...controls };
}
