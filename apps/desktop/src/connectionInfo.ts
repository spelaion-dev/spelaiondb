// What the explorer and the editor show about a connection besides its name.

import type { ConnectionItem } from "./bindings";

/** A connection's tooltip: what the user wrote it is for, then its identifier and `details`
 * (one per line; empty ones are skipped), such as marks that may not fit in the row. */
export function connectionTooltip(connection: ConnectionItem, details: (string | null | undefined)[] = []): string {
  const facts = [connection.id, ...details].filter(Boolean).join("\n");
  return connection.description ? `${connection.description}\n\n${facts}` : facts;
}
