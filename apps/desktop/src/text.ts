// The core gives positions in UTF-8 bytes; JavaScript indexes in UTF-16 units.

const encoder = new TextEncoder();

/** JS index corresponding to a UTF-8 byte offset within `text`. */
export function byteToIndex(text: string, byteOffset: number): number {
  let bytes = 0;
  let index = 0;
  for (const ch of text) {
    if (bytes >= byteOffset) break;
    bytes += encoder.encode(ch).length;
    index += ch.length;
  }
  return index;
}

/** End of the word starting at `index` (to underline an error). */
export function wordEnd(text: string, index: number): number {
  const match = /^[\p{L}\p{N}_."$]+/u.exec(text.slice(index));
  return index + Math.max(1, match ? match[0].length : 1);
}

/** SQL identifier, quoted if needed (for the SQL the UI generates). */
export function quoteIdent(name: string): string {
  return /^[a-z_][a-z0-9_]*$/.test(name) ? name : `"${name.replace(/"/g, '""')}"`;
}
