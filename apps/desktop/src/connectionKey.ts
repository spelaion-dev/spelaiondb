// What identifies a connection when the page names it to the core (ADR-075, SDB-495): its
// folder, as the core listed it (`ConnectionItem.folder`), and its id. Every command that takes
// a connection takes this pair, and the core checks it against what is open.
//
// One folder is open for now, so an id still names one connection in the page, which keys its
// rows and caches by it; those keys become the pair when several folders open (SDB-500).

import type { ConnectionKeyItem_Serialize } from "./bindings";
import { useApp } from "./store";

export type ConnectionKey = ConnectionKeyItem_Serialize;

/** The identity of the listed connection `id`. A connection that is not listed (any more) has
 *  no folder here: the core then refuses it, as it refuses an unknown one. */
export function connectionKey(id: string): ConnectionKey {
  const connection = useApp.getState().connections.find((item) => item.id === id);
  return { folder: connection?.folder ?? null, id };
}

/** Whether two identities name the same connection. */
export function sameConnection(a: { folder?: string | null; id: string }, b: { folder?: string | null; id: string }): boolean {
  return a.id === b.id && (a.folder ?? null) === (b.folder ?? null);
}
