// The editor's snippets (SDB-384): the built-in sel, ins and upd and the user's own, which
// the core stores and checks. A snippet is text the editor inserts when its name is typed
// before Tab, with fields (`${1:name}`) to jump through; it is never run by itself.
//
// This module imports nothing, so `scripts/editor-snippets.test.mjs` loads it with
// `node --test`.

/** A snippet as the editor uses it. */
export interface EditorSnippet {
  /** What is typed before Tab. */
  name: string;
  description: string;
  /** The text inserted, in CodeMirror's snippet syntax. */
  body: string;
  /** One of the app's own (sel, ins, upd), not replaced by a user snippet of that name. */
  builtIn: boolean;
}

/** The app's own snippets; their descriptions are translation keys of the `editor` namespace. */
export const BUILT_IN_SNIPPETS: readonly EditorSnippet[] = [
  { name: "sel", description: "actions.insertSelect", body: "SELECT ${1:columns} FROM ${2:table};${0}", builtIn: true },
  { name: "ins", description: "actions.insertInsert", body: "INSERT INTO ${1:table} (${2:columns}) VALUES (${3:values});${0}", builtIn: true },
  { name: "upd", description: "actions.insertUpdate", body: "UPDATE ${1:table} SET ${2:column} = ${3:value} WHERE ${4:condition};${0}", builtIn: true },
];

/** The built-in snippets and the user's, by name: a user snippet replaces the built-in one of
 *  the same name, and the rest of the user's follow in their order. */
export function allSnippets(user: readonly { name: string; description: string; body: string }[]): EditorSnippet[] {
  const own = new Map(user.map((snippet) => [snippet.name, snippet]));
  const builtIn = BUILT_IN_SNIPPETS.map((snippet) => {
    const replaced = own.get(snippet.name);
    return replaced ? { ...replaced, builtIn: false } : snippet;
  });
  const names = new Set(BUILT_IN_SNIPPETS.map((snippet) => snippet.name));
  return [...builtIn, ...user.filter((snippet) => !names.has(snippet.name)).map((snippet) => ({ ...snippet, builtIn: false }))];
}

/**
 * The snippet whose name ends `before` (the line's text up to the cursor), for Tab: the whole
 * word must be the name, with the same case, and must not follow a letter, a digit or `_`.
 * `start` is how many characters before the cursor the name begins.
 */
export function snippetBefore(before: string, snippets: readonly EditorSnippet[]): { snippet: EditorSnippet; start: number } | null {
  const word = /(?<![A-Za-z0-9_])[A-Za-z_][A-Za-z0-9_]*$/.exec(before);
  if (!word) return null;
  const snippet = snippets.find((candidate) => candidate.name === word[0]);
  return snippet ? { snippet, start: word[0].length } : null;
}

/** The snippets to offer for a word being typed: those whose name starts with it, whatever the
 *  case, an exact name first. */
export function snippetsStarting(word: string, snippets: readonly EditorSnippet[]): EditorSnippet[] {
  const typed = word.toLowerCase();
  if (!typed) return [];
  return snippets
    .filter((snippet) => snippet.name.toLowerCase().startsWith(typed))
    .sort((a, b) => Number(b.name === word) - Number(a.name === word));
}

/** A built-in snippet's keywords in lower case, for users who write keywords so. */
export function lowerKeywords(body: string): string {
  return body.replace(/SELECT|FROM|INSERT INTO|VALUES|UPDATE|SET|WHERE/g, (word) => word.toLowerCase());
}
