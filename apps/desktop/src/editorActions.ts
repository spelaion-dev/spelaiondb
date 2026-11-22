// The SQL editor's keymap derives its keys from the central registry: the user's keys, or the
// defaults (SDB-447). No key of a registry command is written by hand in the editor.
import { COMMANDS, commandKeys, type CommandSpec } from "./commandRegistry.ts";
import { BUILT_IN_SNIPPETS } from "./editorSnippets.ts";

type EditorActionCommand = Extract<(typeof COMMANDS)[number], { editorAction: true; shortcut: string }>;
export type EditorActionId = EditorActionCommand["id"] extends `editor.${infer Action}` ? Action : never;

/** What a key of the editor's keymap does: one of its own chords (`editorAction` in the
 *  registry), or one of the editor's commands the editor runs itself. */
export type EditorKeyTarget =
  | { kind: "action"; id: EditorActionId }
  | { kind: "run" }
  | { kind: "runScript" }
  | { kind: "format" };

/** The editor's commands it runs itself, by id, besides its own chords. */
const EDITOR_RUNS: Record<string, EditorKeyTarget> = {
  "editor.run": { kind: "run" },
  "editor.runScript": { kind: "runScript" },
  "editor.format": { kind: "format" },
};

/** A shortcut as CodeMirror's keymap writes it: Ctrl is Mod (Cmd on macOS), a letter in lower
 *  case, parts joined with "-". */
export function editorKey(shortcut: string): string {
  // A shortcut can end with the minus key (Ctrl+-): split on the "+" before the last character.
  const at = shortcut.lastIndexOf("+", shortcut.length - 2);
  const key = at < 0 ? shortcut : shortcut.slice(at + 1);
  const modifiers = at < 0 ? [] : shortcut.slice(0, at).split("+");
  return [...modifiers.map((part) => (part === "Ctrl" ? "Mod" : part)), key.length === 1 ? key.toLowerCase() : key].join("-");
}

/** What a registry command does as a key of the editor's keymap, or `null` when the keymap
 *  does not bind it: not the editor's, its keys kept by the editor itself (the clipboard). */
export function editorKeyTarget(command: CommandSpec): EditorKeyTarget | null {
  if (command.scope !== "editor" || command.fixedKeys) return null;
  if (command.editorAction) return { kind: "action", id: command.id.slice("editor.".length) as EditorActionId };
  return EDITOR_RUNS[command.id] ?? null;
}

/** The editor's keymap from the registry, with each command's keys now (the user's, or the
 *  default): every editor command with keys that the editor binds. */
export function editorKeyBindings(): { id: string; shortcut: string; key: string; target: EditorKeyTarget }[] {
  return (COMMANDS as readonly CommandSpec[]).flatMap((command) => {
    const shortcut = commandKeys(command);
    const target = editorKeyTarget(command);
    return shortcut && target ? [{ id: command.id, shortcut, key: editorKey(shortcut), target }] : [];
  });
}

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
