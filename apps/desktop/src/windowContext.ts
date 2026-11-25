// What this window shows (SDB-30, ADR-083), as the core's registry says it: the main window,
// a connection's window or a tab's results window. Read once before the first render
// (`main.tsx`) and never changed: a window shows the same thing until it closes.
//
// Every window is a view over the one core, so session keys must never collide between
// windows: a window other than the main one prefixes its keys with its label
// (`connection-2/tab-1`, `connection-2/meta:shop`). The core keeps keys opaque and records
// which window opened each session, to close only that window's sessions with it.

import type { WindowContextItem } from "./bindings";

let current: WindowContextItem = { kind: "main" };

export function setWindowContext(context: WindowContextItem) {
  current = context;
}

export function windowContext(): WindowContextItem {
  return current;
}

/** The label of this window, as the core named it. */
export function windowLabel(): string {
  switch (current.kind) {
    case "main":
      return "main";
    case "connection":
    case "results":
      return current.label;
    default:
      return unknownWindow(current);
  }
}

/** What this window's session keys start with: nothing in the main window. */
export function sessionPrefix(): string {
  return current.kind === "main" ? "" : `${current.label}/`;
}

/** The explorer's session on a connection, in this window. */
export function explorerSession(connectionId: string): string {
  return `${sessionPrefix()}meta:${connectionId}`;
}

/** The connection whose explorer session in this window `session` is, or null. */
export function explorerConnectionOf(session: string): string | null {
  const prefix = `${sessionPrefix()}meta:`;
  return session.startsWith(prefix) ? session.slice(prefix.length) : null;
}

/** The connection this window was opened for, or null in the main window. */
export function windowConnection(): string | null {
  return current.kind === "connection" ? current.connection.id : null;
}

function unknownWindow(context: never): never {
  throw new Error(`unknown window: ${JSON.stringify(context)}`);
}
