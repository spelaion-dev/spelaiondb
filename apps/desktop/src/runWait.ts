// A Run pressed while an editor tab cannot start one (SDB-525). It is never dropped without a
// word: what blocks the tab and ends on its own (the session opening, the database list after
// connecting, the history being saved, the view being restored) makes it wait, and it runs once,
// the text as it was when pressed, as soon as the tab can run. What the user is doing (the
// folder changing) is said, never waited for. A connection that fails drops the waiting run and
// says so: nothing runs on its own after a reconnect (ADR-028, ADR-037), and a waiting run
// belongs to the session that was opening when it was pressed. The decision is the tab's
// (EditorTab.tsx); these are its states and transitions, tested by scripts/run-wait.test.mjs.

/** What keeps the tab from starting a run now, if anything; the user's own doing first. */
export type RunBlock =
  | { kind: "free" }
  | { kind: "folder" }
  | { kind: "connecting" }
  | { kind: "databases" }
  | { kind: "history" }
  | { kind: "restoring" };

/** What blocks it and ends on its own: a Run pressed meanwhile waits for it. */
export type WaitFor = Extract<RunBlock["kind"], "connecting" | "databases" | "history" | "restoring">;

export interface BlockCauses {
  folder: boolean;
  connecting: boolean;
  databases: boolean;
  history: boolean;
  restoring: boolean;
}

export function runBlock(causes: BlockCauses): RunBlock {
  if (causes.folder) return { kind: "folder" };
  if (causes.connecting) return { kind: "connecting" };
  if (causes.databases) return { kind: "databases" };
  if (causes.history) return { kind: "history" };
  if (causes.restoring) return { kind: "restoring" };
  return { kind: "free" };
}

/** At most one run waits, with what it waits for now (said in the tab) and whether the tab had
 *  a session open or opening since it was pressed: that session going away drops it. */
export type WaitingRun<T> = { kind: "none" } | { kind: "waiting"; run: T; waitFor: WaitFor; opened: boolean };

/** The tab's session now: a lost or closed one is `none`. */
export type SessionNow = "none" | "connecting" | "connected";

export const NOT_WAITING: WaitingRun<never> = { kind: "none" };

/** What a Run pressed now does. */
export type Press<T> =
  | { kind: "start" }
  | { kind: "wait"; waiting: WaitingRun<T> & { kind: "waiting" } }
  /** A run already waits: a second press adds nothing, the first still says it waits. */
  | { kind: "alreadyWaiting" }
  /** The user is changing the folder: said, never waited for. */
  | { kind: "refused"; because: "folder" };

export function press<T>(block: RunBlock, waiting: WaitingRun<T>, run: T, session: SessionNow): Press<T> {
  switch (block.kind) {
    case "free":
      return { kind: "start" };
    case "folder":
      return { kind: "refused", because: "folder" };
    case "connecting":
    case "databases":
    case "history":
    case "restoring":
      return waiting.kind === "waiting" ? { kind: "alreadyWaiting" } : { kind: "wait", waiting: { kind: "waiting", run, waitFor: block.kind, opened: session !== "none" } };
    default:
      return unknownBlock(block);
  }
}

/** What a waiting run does once what blocks the tab, or its session, changed. */
export type Settle<T> =
  | { kind: "keep"; waiting: WaitingRun<T> }
  | { kind: "start"; run: T }
  | { kind: "drop"; because: "connectionFailed" | "folder" };

export function settle<T>(waiting: WaitingRun<T>, block: RunBlock, session: SessionNow): Settle<T> {
  if (waiting.kind === "none") return { kind: "keep", waiting };
  // Never into another session than the one that was opening (ADR-028).
  if (waiting.opened && session === "none") return { kind: "drop", because: "connectionFailed" };
  const opened = waiting.opened || session !== "none";
  switch (block.kind) {
    case "free":
      // It runs, once: in the session that opened, or connecting first as any Run on a tab
      // without one does.
      return { kind: "start", run: waiting.run };
    case "folder":
      return { kind: "drop", because: "folder" };
    case "connecting":
    case "databases":
    case "history":
    case "restoring":
      return { kind: "keep", waiting: waiting.waitFor === block.kind && waiting.opened === opened ? waiting : { ...waiting, waitFor: block.kind, opened } };
    default:
      return unknownBlock(block);
  }
}

/** The tab's notice while a run waits. */
export function waitingKey(waitFor: WaitFor): string {
  switch (waitFor) {
    case "connecting":
      return "runWait.connecting";
    case "databases":
      return "runWait.databases";
    case "history":
      return "runWait.history";
    case "restoring":
      return "runWait.restoring";
    default:
      return unknownBlock(waitFor);
  }
}

/** The tab's notice when a pressed run did not start. */
export function notRunKey(because: "connectionFailed" | "folder", waited: boolean): string {
  switch (because) {
    case "connectionFailed":
      return "runWait.connectionFailed";
    case "folder":
      return waited ? "runWait.folderChanged" : "runWait.folderChanging";
    default:
      return unknownBlock(because);
  }
}

function unknownBlock(value: never): never {
  throw new Error(`unknown run block: ${JSON.stringify(value)}`);
}
