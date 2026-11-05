// How the explorer orders the names in its tree (Settings → Explorer). Plain functions with no
// imports, so `scripts/explorer-sort.test.mjs` runs them with Node alone.

export interface SortPreferences {
  /** Alphabetical order; otherwise the order the database returns. */
  sortAlphabetically: boolean;
  /** Ignore case (and compare the numbers in names as numbers: `t2` before `t10`). */
  sortIgnoreCase: boolean;
}

function byCodePoint(a: string, b: string): number {
  return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * The items in the explorer's order, as a new array. Case-sensitive order compares code
 * points, as DBeaver does (`Z` before `a`). Items that compare equal keep their order, so
 * overloads of a function stay as the database lists them.
 */
export function sortByName<T>(
  items: readonly T[],
  name: (item: T) => string,
  preferences: SortPreferences,
  language: string,
): T[] {
  if (!preferences.sortAlphabetically) return [...items];
  let compare = byCodePoint;
  if (preferences.sortIgnoreCase) {
    const collator = new Intl.Collator(language, { sensitivity: "accent", numeric: true });
    compare = (a, b) => collator.compare(a, b) || byCodePoint(a, b);
  }
  return [...items].sort((x, y) => compare(name(x), name(y)));
}
