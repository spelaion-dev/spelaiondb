// What the explorer and the editor show about a connection besides its name.

import type { ConnectionItem } from "./bindings";

/** A connection's tooltip: what the user wrote it is for, then its identifier. */
export function connectionTooltip(connection: ConnectionItem): string {
  return connection.description ? `${connection.description}\n\n${connection.id}` : connection.id;
}
