// What this window shows (SDB-30, ADR-083), as the core's registry says it: the main window,
// a connection's window or a tab's results window. Read once before the first render
// (`main.tsx`) and never changed: a window shows the same thing until it closes.
//
// Every window is a view over the one core, so session keys must never collide between
// windows: a window other than the main one prefixes its keys with its label
// (`connection-2/tab-1`, `connection-2/meta:[null,"~/shop"]`). The core keeps keys opaque and records
// which window opened each session, to close only that window's sessions with it.

import type { WindowContextItem } from "./bindings";
import { connectionOfTag, connectionTag, type ConnectionKey } from "./connectionIdentity.ts";

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

/** The explorer's session on a connection, in this window: keyed by its folder and its id
 *  (ADR-075), so two folders' connections of the same id never share one. */
export function explorerSession(connection: ConnectionKey): string {
  return `${sessionPrefix()}meta:${connectionTag(connection)}`;
}

/** The connection whose explorer session in this window `session` is, or null. */
export function explorerConnectionOf(session: string): ConnectionKey | null {
  const prefix = `${sessionPrefix()}meta:`;
  return session.startsWith(prefix) ? connectionOfTag(session.slice(prefix.length)) : null;
}

/** The connection this window was opened for, or null in the main window. */
export function windowConnection(): ConnectionKey | null {
  return current.kind === "connection" ? { folder: current.connection.folder ?? null, id: current.connection.id } : null;
}

function unknownWindow(context: never): never {
  throw new Error(`unknown window: ${JSON.stringify(context)}`);
}
