// Editor-local key chords derive their names and shortcuts from the central registry.
import { COMMANDS, menuCommands } from "./commandRegistry.ts";

type EditorActionCommand = Extract<(typeof COMMANDS)[number], { menus: readonly ["editor"]; shortcut: string }>;
export type EditorActionId = EditorActionCommand["id"] extends `editor.${infer Action}` ? Action : never;
export const EDITOR_ACTIONS = menuCommands("editor").filter((command) => command.shortcut !== undefined).map((command) => ({
  id: command.id.slice("editor.".length) as EditorActionId,
  shortcut: command.shortcut!,
  key: command.shortcut!.split("+").map((part, index, parts) => part === "Ctrl" ? "Mod" : index === parts.length - 1 ? part.toLowerCase() : part).join("-"),
}));

/** Explicit tab-stop ordering keeps the last editable field active until the final stop. */
export const EDITOR_SNIPPETS = {
  insertSelect: "SELECT ${1:columns} FROM ${2:table};${0}",
  insertInsert: "INSERT INTO ${1:table} (${2:columns}) VALUES (${3:values});${0}",
  insertUpdate: "UPDATE ${1:table} SET ${2:column} = ${3:value} WHERE ${4:condition};${0}",
} as const;

/** Text conversion only: it never runs SQL, and remains one undo operation. */
export function quotedLines(text: string, engine: string | null = null): string {
  return text.split(/\r\n|\r|\n/).map((line) => {
    const escaped = engine === "mysql" || engine === "mariadb" ? line.replaceAll("\\", "\\\\") : line;
    return (engine === "sqlserver" ? "N'" : "'") + escaped.replaceAll("'", "''") + "'";
  }).join(", ");
}
