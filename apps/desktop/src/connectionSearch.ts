// The explorer's connection search: finds a connection by its name, tags, type, host or
// description, forgiving a typo. Each word typed must match one of them (in any order): the
// palette's fuzzy match (letters in order, `fuzzy.ts`, starting at a word: "ord" finds "Orders"
// but not "Postgres (Docker)", whose o, r and d are scattered) on the short fields, a plain substring
// in the description (letters spread over a paragraph would match almost anything), and in any
// of them a word one or two edits away ("ordres", "postgers") from a word of the field. Pure,
// so `scripts/connection-search.test.mjs` runs it.

import { fuzzyMatch } from "./fuzzy.ts";

export interface SearchableConnection {
  name: string;
  description?: string | null;
  tags: string[];
  host?: string | null;
  /** The connection type's name (Production, Test…), if it has one. */
  typeName?: string | null;
}

const WORD_SEPARATORS = /[\s_\-./\\:,;()[\]{}@#·]+/;

function lower(text: string): string {
  return text.toLocaleLowerCase();
}

const LETTER_OR_DIGIT = /[\p{L}\p{N}]/u;

/** Whether the character at `index` (in code points) starts a word: the text's start, after a
 * separator, or a capital after a lower-case letter. */
function startsWord(chars: string[], index: number): boolean {
  const previous = chars[index - 1];
  const current = chars[index] ?? "";
  if (previous === undefined || !LETTER_OR_DIGIT.test(previous)) return true;
  return previous === previous.toLowerCase() && previous !== previous.toUpperCase() && current !== current.toLowerCase();
}

/** Optimal string alignment distance (insertions, deletions, substitutions and swaps of two
 * neighbours), giving up beyond `limit`. */
export function editDistance(a: string, b: string, limit: number): number {
  const x = [...a];
  const y = [...b];
  if (Math.abs(x.length - y.length) > limit) return limit + 1;
  let previous2: number[] = [];
  let previous = Array.from({ length: y.length + 1 }, (_, j) => j);
  for (let i = 1; i <= x.length; i++) {
    const current = [i];
    let rowBest = i;
    for (let j = 1; j <= y.length; j++) {
      const cost = x[i - 1] === y[j - 1] ? 0 : 1;
      let value = Math.min(
        (previous[j] ?? Infinity) + 1,
        (current[j - 1] ?? Infinity) + 1,
        (previous[j - 1] ?? Infinity) + cost,
      );
      if (i > 1 && j > 1 && x[i - 1] === y[j - 2] && x[i - 2] === y[j - 1]) {
        value = Math.min(value, (previous2[j - 2] ?? Infinity) + 1);
      }
      current.push(value);
      rowBest = Math.min(rowBest, value);
    }
    if (rowBest > limit) return limit + 1;
    previous2 = previous;
    previous = current;
  }
  return previous[y.length] ?? limit + 1;
}

/** Edits allowed for a word of this length: none for short words (they would match too much). */
function allowedEdits(length: number): number {
  if (length < 4) return 0;
  if (length < 8) return 1;
  return 2;
}

/** Whether a typed word is a typo of a word in the text: the whole word, or its start when the
 * word typed is still being written ("ordr" → "orders"). */
function typoOf(word: string, text: string): boolean {
  const edits = allowedEdits([...word].length);
  if (edits === 0) return false;
  for (const candidate of lower(text).split(WORD_SEPARATORS)) {
    if (candidate === "") continue;
    if (editDistance(word, candidate, edits) <= edits) return true;
    const start = [...candidate].slice(0, [...word].length).join("");
    if (start !== candidate && editDistance(word, start, edits) <= edits) return true;
  }
  return false;
}

/** How well one typed word matches a field; null if it does not. */
function wordScore(word: string, text: string, loose: boolean): number | null {
  if (text === "") return null;
  if (loose) {
    const match = fuzzyMatch(word, text);
    if (match && startsWord([...text], match.positions[0] ?? 0)) return match.score;
  } else if (lower(text).includes(word)) {
    return 1;
  }
  return typoOf(word, text) ? 0.5 : null;
}

/** How well the query matches a connection, higher is better; null if a word typed matches
 * none of its fields. An empty query matches every connection. */
export function connectionMatch(query: string, connection: SearchableConnection): number | null {
  const words = lower(query).split(/\s+/).filter((word) => word !== "");
  if (words.length === 0) return 0;
  // The name counts most, then the labels people choose, then where it points.
  const fields: { text: string; weight: number; loose: boolean }[] = [
    { text: connection.name, weight: 4, loose: true },
    ...connection.tags.map((tag) => ({ text: tag, weight: 3, loose: true })),
    { text: connection.typeName ?? "", weight: 2, loose: true },
    { text: connection.host ?? "", weight: 2, loose: true },
    { text: connection.description ?? "", weight: 1, loose: false },
  ];
  let total = 0;
  for (const word of words) {
    let best: number | null = null;
    for (const field of fields) {
      const score = wordScore(word, field.text, field.loose);
      if (score !== null && (best === null || score * field.weight > best)) best = score * field.weight;
    }
    if (best === null) return null;
    total += best;
  }
  return total;
}
