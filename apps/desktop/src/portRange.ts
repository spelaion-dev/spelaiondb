// The Network settings' local port range form (components/Settings.tsx), as a reducer so its
// timing can be tested without the app (scripts/port-range.test.mjs). The stored range is read
// once when the form opens, and again on Retry. A reply never overwrites what the user typed,
// and only the latest request's reply counts: a slow first reply cannot erase a value typed
// after it was asked for (SDB-96).

export interface PortRange {
  min: number;
  max: number;
}

export type PortRangeField = "min" | "max";

export interface PortRangeState {
  min: string;
  max: string;
  /** Fields the user has typed in since the form opened or last saved. */
  edited: Record<PortRangeField, boolean>;
  /** The latest read of the stored range: only its reply counts. */
  request: number;
  load: { kind: "loading" } | { kind: "ready" } | { kind: "failed"; message: string };
  status: { kind: "idle" } | { kind: "saved" } | { kind: "error"; message: string };
}

export type PortRangeAction =
  /** Read the stored range again (Retry). */
  | { type: "reload" }
  | { type: "loaded"; request: number; range: PortRange | null }
  | { type: "loadFailed"; request: number; message: string }
  | { type: "typed"; field: PortRangeField; value: string }
  | { type: "saved"; range: PortRange | null }
  | { type: "saveFailed"; message: string };

/** The form when it opens: reading the stored range (request 0). */
export const initialPortRange: PortRangeState = {
  min: "",
  max: "",
  edited: { min: false, max: false },
  request: 0,
  load: { kind: "loading" },
  status: { kind: "idle" },
};

export function portRangeReducer(state: PortRangeState, action: PortRangeAction): PortRangeState {
  switch (action.type) {
    case "reload":
      return { ...state, request: state.request + 1, load: { kind: "loading" } };
    case "loaded": {
      if (action.request !== state.request) return state;
      const stored = (value: number | undefined) => (value === undefined ? "" : String(value));
      return {
        ...state,
        min: state.edited.min ? state.min : stored(action.range?.min),
        max: state.edited.max ? state.max : stored(action.range?.max),
        load: { kind: "ready" },
      };
    }
    case "loadFailed":
      if (action.request !== state.request) return state;
      return { ...state, load: { kind: "failed", message: action.message } };
    case "typed":
      return {
        ...state,
        [action.field]: action.value,
        edited: { ...state.edited, [action.field]: true },
        status: { kind: "idle" },
      };
    case "saved":
      // What is shown is now what is stored.
      return {
        ...state,
        min: action.range ? String(action.range.min) : "",
        max: action.range ? String(action.range.max) : "",
        edited: { min: false, max: false },
        status: { kind: "saved" },
      };
    case "saveFailed":
      // The typed values stay, to be corrected.
      return { ...state, status: { kind: "error", message: action.message } };
  }
}

/** A field's port, or `null` when it is empty. The core checks the range itself. */
export function typedPort(text: string): number | null {
  return text.trim() === "" ? null : Math.trunc(Number(text));
}

/** The range the fields hold, when both have a port. */
export function typedRange(state: PortRangeState): PortRange | null {
  const min = typedPort(state.min);
  const max = typedPort(state.max);
  return min === null || max === null ? null : { min, max };
}

/** The fields and buttons can be used: the stored range is not being read. */
export function portRangeReady(state: PortRangeState): boolean {
  return state.load.kind !== "loading";
}
