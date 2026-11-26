// A connection's identity in the page (ADR-075, SDB-495): the pure helpers are in
// `connectionIdentity.ts`; this adds what needs the listed connections, for what still holds a
// bare id (a launch, the MCP page).

import { useApp } from "./store";
import type { ConnectionKey } from "./connectionIdentity.ts";

export * from "./connectionIdentity.ts";

/** The identity of the listed connection `id`. A connection that is not listed (any more) has
 *  no folder here: the core then refuses it, as it refuses an unknown one. */
export function connectionKey(id: string): ConnectionKey {
  const connection = useApp.getState().connections.find((item) => item.id === id);
  return { folder: connection?.folder ?? null, id };
}
