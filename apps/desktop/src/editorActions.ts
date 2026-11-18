// Editor-local key chords derive their names and shortcuts from the central registry.
import { COMMANDS, menuCommands } from "./commandRegistry.ts";
import { BUILT_IN_SNIPPETS } from "./editorSnippets.ts";

type EditorActionCommand = Extract<(typeof COMMANDS)[number], { menus: readonly ["editor"]; shortcut: string }>;
export type EditorActionId = EditorActionCommand["id"] extends `editor.${infer Action}` ? Action : never;
export const EDITOR_ACTIONS = menuCommands("editor").filter((command) => command.shortcut !== undefined).map((command) => ({
  id: command.id.slice("editor.".length) as EditorActionId,
  shortcut: command.shortcut!,
  key: command.shortcut!.split("+").map((part, index, parts) => part === "Ctrl" ? "Mod" : index === parts.length - 1 && part.length === 1 ? part.toLowerCase() : part).join("-"),
}));

/** The snippet each "insert snippet" command inserts, by its name (sel, ins, upd): the
 *  built-in one, or the user's own of that name (SDB-384). */
export const SNIPPET_COMMANDS = { insertSelect: "sel", insertInsert: "ins", insertUpdate: "upd" } as const;

const builtIn = (name: string) => BUILT_IN_SNIPPETS.find((snippet) => snippet.name === name)!.body;
/** The built-in snippets' text. Explicit tab-stop ordering keeps the last editable field
 *  active until the final stop. */
export const EDITOR_SNIPPETS = {
  insertSelect: builtIn("sel"),
  insertInsert: builtIn("ins"),
  insertUpdate: builtIn("upd"),
} as const;

/** Text conversion only: it never runs SQL, and remains one undo operation. */
export function quotedLines(text: string, engine: string | null = null): string {
  return text.split(/\r\n|\r|\n/).map((line) => {
    const escaped = engine === "mysql" || engine === "mariadb" ? line.replaceAll("\\", "\\\\") : line;
    return (engine === "sqlserver" ? "N'" : "'") + escaped.replaceAll("'", "''") + "'";
  }).join(", ");
}
