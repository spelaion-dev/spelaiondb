// What the explorer and the editor show about a connection besides its name.

import type { ConnectionItem } from "./bindings";

/** A connection's tooltip: what the user wrote it is for, then its identifier. */
export function connectionTooltip(connection: ConnectionItem): string {
  return connection.description ? `${connection.description}\n\n${connection.id}` : connection.id;
}

/** The core's limits (`ConnectionFile::MAX_TAGS` and `MAX_TAG`); the core checks them again. */
export const MAX_TAGS = 20;
export const MAX_TAG_LENGTH = 32;

/** Tags are the same when they differ only in case, as the core compares them. */
export function sameTag(a: string, b: string): boolean {
  return a.toLocaleLowerCase() === b.toLocaleLowerCase();
}

/** `tags` plus the typed ones, cleaned as the core will: trimmed, single spaces, no repeats. */
export function addTags(tags: string[], typed: string[]): string[] {
  const next = [...tags];
  for (const raw of typed) {
    const tag = raw.trim().split(/\s+/).join(" ");
    if (tag && next.length < MAX_TAGS && !next.some((other) => sameTag(other, tag))) next.push(tag);
  }
  return next;
}

/** Every tag used by these connections, once (the first spelling), sorted. */
export function tagsInUse(connections: ConnectionItem[]): string[] {
  const all: string[] = [];
  for (const c of connections) for (const tag of c.tags) if (!all.some((other) => sameTag(other, tag))) all.push(tag);
  return all.sort((a, b) => a.localeCompare(b, undefined, { sensitivity: "base" }));
}

/** Whether the connection has every one of the tags (an empty list keeps all). */
export function hasTags(connection: ConnectionItem, tags: string[]): boolean {
  return tags.every((tag) => connection.tags.some((own) => sameTag(own, tag)));
}
