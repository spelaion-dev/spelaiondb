// What identifies a connection in the page (ADR-075, SDB-495): its folder, as the core listed
// it (`ConnectionItem.folder`, none for a personal connection), and its id. Every command that
// takes a connection takes this pair, and the core checks it against what is open. The
// explorer, its sessions, the catalog it publishes, and the dialogs and pickers it opens hold
// the pair too, never the bare id, so two folders' connections of the same id never meet.
//
// A tab still names its connection by the bare id of the one folder open: tabs gain the folder
// with the card that makes them belong to it (ADR-075 point 7). Where the explorer meets a tab,
// it goes through `tabOnConnection` and `tabKey` here, and `connectionKey` (`connectionKey.ts`).
//
// Pure (no store, no runtime imports), so `tabState.ts` and its tests can use it.

import type { ConnectionItem } from "./api";
import type { ConnectionKeyItem_Serialize } from "./bindings";

export type ConnectionKey = ConnectionKeyItem_Serialize;

/**
 * An explorer key on one of the other databases of a PostgreSQL connection that opens several
 * (ADR-074, SDB-504): the connection's pair and, beside it, never inside its id, the database.
 * Its nodes take it as their identity, so their session (`explorerSession`) and their entries
 * of the catalog (`catalogKey`) are that database's, through `connectionTag`. Everything keyed
 * by the connection alone (`sameConnection`, its filters, the commands' connection) ignores it.
 */
type DatabaseKey = ConnectionKey & { database?: string };

/** `key` on `database`, one of its connection's other databases; `null` is its own. */
export function onDatabase(key: ConnectionKey, database: string | null): ConnectionKey {
  const scoped: DatabaseKey = database === null ? { folder: key.folder, id: key.id } : { folder: key.folder, id: key.id, database };
  return scoped;
}

/** The other database an explorer key is on (`onDatabase`), or null for the connection's own. */
export function databaseOf(key: ConnectionKey): string | null {
  return (key as DatabaseKey).database ?? null;
}

/** The identity of a listed connection. */
export function keyOf(connection: Pick<ConnectionItem, "folder" | "id">): ConnectionKey {
  return { folder: connection.folder, id: connection.id };
}

/** Whether two identities name the same connection. */
export function sameConnection(a: { folder?: string | null; id: string }, b: { folder?: string | null; id: string }): boolean {
  return a.id === b.id && (a.folder ?? null) === (b.folder ?? null);
}

/** The listed connection `key` names, if it is listed. */
export function listedConnection(connections: ConnectionItem[], key: ConnectionKey): ConnectionItem | undefined {
  return connections.find((item) => sameConnection(item, key));
}

/** The pair as one text, for what can only be keyed by text: the page's maps and sets, React
 *  keys, a picker's values and the explorer's session keys. Both halves are kept whole and
 *  apart (JSON), so no folder and id can ever give the text of another; read back only by
 *  `connectionOfTag`, never taken apart by hand. */
export function connectionTag(key: ConnectionKey): string {
  const database = databaseOf(key);
  // The connection's own database keeps the pair's text, as before databases had a level.
  return JSON.stringify(database === null ? [key.folder ?? null, key.id] : [key.folder ?? null, key.id, database]);
}

/** The pair `connectionTag` wrote, or null for any other text. */
export function connectionOfTag(tag: string): ConnectionKey | null {
  try {
    const value: unknown = JSON.parse(tag);
    if (Array.isArray(value) && value.length === 2 && (value[0] === null || typeof value[0] === "string") && typeof value[1] === "string") {
      return { folder: value[0], id: value[1] };
    }
    if (Array.isArray(value) && value.length === 3 && (value[0] === null || typeof value[0] === "string") && typeof value[1] === "string" && typeof value[2] === "string") {
      return onDatabase({ folder: value[0], id: value[1] }, value[2]);
    }
  } catch {
    // Not a tag.
  }
  return null;
}

/** Whether a set of identities holds `key`. */
export function holdsConnection(keys: readonly ConnectionKey[], key: ConnectionKey): boolean {
  return keys.some((item) => sameConnection(item, key));
}

/** `keys` with `key` added (once) or taken out. */
export function withConnection(keys: ConnectionKey[], key: ConnectionKey, present: boolean): ConnectionKey[] {
  if (present) return holdsConnection(keys, key) ? keys : [...keys, key];
  return keys.filter((item) => !sameConnection(item, key));
}

/** Whether a tab's connection (its bare id, of the one folder open) is `key`. */
export function tabOnConnection(connectionId: string | null, key: ConnectionKey): boolean {
  return connectionId !== null && connectionId === key.id;
}

/** What a tab opened from the explorer on `key` names its connection by. */
export function tabKey(key: ConnectionKey): string {
  return key.id;
}
