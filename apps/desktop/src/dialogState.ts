// States that exist only in the page (ADR-060): a value being loaded, a dialog's "preview, then
// apply", and the phase of a job a dialog runs. Each is one union, never flags beside each other,
// and its transitions are here, as reducers, so they can be tested without the app
// (scripts/dialog-state.test.mjs). The hooks that drive them are in `useDialogState.ts`.
//
// None of this decides anything: what may run is the core's check ("applied only as it was
// shown", ADR-047; the guard, ADR-008). These states only say what the dialog shows.

/** A value the page asked the core for. */
export type Loaded<T> =
  | { kind: "idle" }
  | { kind: "loading" }
  | { kind: "ready"; value: T }
  | { kind: "failed"; message: string };

/** The text of an error the core or the browser gave, as it gave it. */
export function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** The value, when it is loaded. */
export function loadedValue<T>(loaded: Loaded<T>): T | null {
  switch (loaded.kind) {
    case "ready":
      return loaded.value;
    case "idle":
    case "loading":
    case "failed":
      return null;
    default: {
      const unexpected: never = loaded;
      return unexpected;
    }
  }
}

// ---------------------------------------------------------------------------------------------
// Preview, then apply

/** The core's last answer to a preview, and the form (by its key) it answered. */
export type PreviewAnswer<P> = {
  key: string;
  preview: { kind: "ready"; value: P } | { kind: "failed"; message: string };
};

/** What became of Apply. `applied` keeps a note for a dialog that stays open after it. */
export type ApplyPhase =
  | { kind: "idle" }
  | { kind: "applying" }
  | { kind: "applied"; note: string }
  | { kind: "failed"; message: string };

export interface PreviewApplyState<P> {
  answer: PreviewAnswer<P> | null;
  apply: ApplyPhase;
}

export type PreviewApplyAction<P> =
  | { type: "answered"; key: string; value: P }
  | { type: "refused"; key: string; message: string }
  /** The form has nothing to preview: the last answer no longer counts. */
  | { type: "cleared" }
  | { type: "applying" }
  | { type: "applied"; note: string }
  | { type: "applyFailed"; message: string }
  /** The note of the last apply is no longer about what the form shows; a failure stays. */
  | { type: "noteRead" };

export function initialPreviewApply<P>(): PreviewApplyState<P> {
  return { answer: null, apply: { kind: "idle" } };
}

export function previewApplyReducer<P>(state: PreviewApplyState<P>, action: PreviewApplyAction<P>): PreviewApplyState<P> {
  switch (action.type) {
    case "answered":
      return { ...state, answer: { key: action.key, preview: { kind: "ready", value: action.value } } };
    case "refused":
      return { ...state, answer: { key: action.key, preview: { kind: "failed", message: action.message } } };
    case "cleared":
      return state.answer === null ? state : { ...state, answer: null };
    case "applying":
      return { ...state, apply: { kind: "applying" } };
    case "applied":
      return { ...state, apply: { kind: "applied", note: action.note } };
    case "applyFailed":
      return { ...state, apply: { kind: "failed", message: action.message } };
    case "noteRead":
      return state.apply.kind === "applied" ? { ...state, apply: { kind: "idle" } } : state;
    default: {
      const unexpected: never = action;
      return unexpected;
    }
  }
}

/**
 * The preview of the form as it is now (`key`; null when there is nothing to preview). An answer
 * to an earlier form is not this one's, so the dialog waits for its own; a refusal stays shown
 * until the next answer, so the reason does not flicker away while the user types.
 */
export function previewFor<P>(state: PreviewApplyState<P>, key: string | null): Loaded<P> {
  if (key === null) return { kind: "idle" };
  const answer = state.answer;
  if (answer === null) return { kind: "loading" };
  switch (answer.preview.kind) {
    case "ready":
      return answer.key === key ? answer.preview : { kind: "loading" };
    case "failed":
      return answer.preview;
    default: {
      const unexpected: never = answer.preview;
      return unexpected;
    }
  }
}

/** The last preview the core answered, whichever form it was for (a dialog that keeps showing it
 *  while a new one is asked for). */
export function lastPreview<P>(state: PreviewApplyState<P>): Loaded<P> {
  return state.answer === null ? { kind: "loading" } : state.answer.preview;
}

/** The script a preview shows: its SQL, the core's refusal as it gave it, or "…" while it is
 *  asked for. */
export function scriptText(preview: Loaded<{ sql: string }>): string {
  switch (preview.kind) {
    case "ready":
      return preview.value.sql;
    case "failed":
      return preview.message;
    case "idle":
    case "loading":
      return "…";
    default: {
      const unexpected: never = preview;
      return unexpected;
    }
  }
}

// ---------------------------------------------------------------------------------------------
// A job's phase

/**
 * The phase of a job a dialog runs in the core (an export, an import, a backup, a file run):
 * not started, running with the id the core keys it by (to cancel it; null when it cannot be
 * cancelled) and its progress, or ended. How it ended is read once from the core's `JobOutcome`
 * into what the dialog shows: `done` (`D`) or `failed` (`F`).
 */
export type JobPhase<P, D, F> =
  | { kind: "idle" }
  | { kind: "running"; id: number | null; started: number; progress: P }
  | ({ kind: "done" } & D)
  | ({ kind: "failed" } & F);

export type JobAction<P, D, F> =
  | { type: "started"; id: number | null; started: number; progress: P }
  /** Progress of the run `id`; dropped once it has ended. */
  | { type: "progressed"; id: number | null; progress: P }
  | { type: "done"; done: D }
  | { type: "failed"; failed: F }
  /** Back to the form: a new run can start. */
  | { type: "reset" };

export function jobReducer<P, D, F>(phase: JobPhase<P, D, F>, action: JobAction<P, D, F>): JobPhase<P, D, F> {
  switch (action.type) {
    case "started":
      return { kind: "running", id: action.id, started: action.started, progress: action.progress };
    case "progressed":
      return phase.kind === "running" && phase.id === action.id ? { ...phase, progress: action.progress } : phase;
    case "done":
      return { ...action.done, kind: "done" };
    case "failed":
      return { ...action.failed, kind: "failed" };
    case "reset":
      return { kind: "idle" };
    default: {
      const unexpected: never = action;
      return unexpected;
    }
  }
}
