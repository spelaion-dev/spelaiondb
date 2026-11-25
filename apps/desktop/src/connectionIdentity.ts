// What identifies a connection in the page (ADR-075, SDB-495): its folder, as the core listed
// it (`ConnectionItem.folder`, none for a personal connection), and its id. Every command that
// takes a connection takes this pair, and the core checks it against what is open. The
// explorer, its sessions, the catalog it publishes, and the dialogs and pickers it opens hold
// the pair too, never the bare id, so two folders' connections of the same id never meet.
//
// A tab holds both halves too (`connectionId` and `connectionFolder`, ADR-075 point 7, SDB-502):
// `tabConnection` gives its identity, and `tabOnConnection` and `stillOnConnection` compare it.
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
export function listedConnection(connections: ConnectionItem[], key: { folder?: string | null; id: string }): ConnectionItem | undefined {
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

/** What a tab knows of its connection: its id and the folder it is from (ADR-075). */
export interface TabConnection {
  connectionId: string | null;
  connectionFolder: string | null;
}

/** A tab's connection as its identity: its id and its folder; null for a tab with none. */
export function tabConnection(tab: TabConnection): ConnectionKey | null {
  return tab.connectionId === null ? null : { folder: tab.connectionFolder, id: tab.connectionId };
}

/** The identity of a tab's connection `id` (a tab that has one), with the tab's folder. */
export function tabConnectionOf(id: string, folder: string | null): ConnectionKey {
  return { folder, id };
}

/** Whether a tab is still on `key` (a connection, or none): a check after an await that the
 *  user did not change the tab's connection meanwhile. */
export function stillOnConnection(tab: TabConnection, key: ConnectionKey | null): boolean {
  const own = tabConnection(tab);
  return own === null || key === null ? own === key : sameConnection(own, key);
}

/** Whether a tab's connection is `key`: the same folder and the same id. */
export function tabOnConnection(tab: TabConnection, key: ConnectionKey): boolean {
  const own = tabConnection(tab);
  return own !== null && sameConnection(own, key);
}
