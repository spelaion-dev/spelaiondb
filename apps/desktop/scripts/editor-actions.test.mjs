import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import { EDITOR_SNIPPETS, editorKey, editorKeyBindings, editorKeyTarget, quotedLines } from "../src/editorActions.ts";
import { COMMANDS } from "../src/commandRegistry.ts";
import { EditorState } from "@codemirror/state";
import { snippet, nextSnippetField, prevSnippetField } from "@codemirror/autocomplete";
import { leadingCommas } from "../src/editorFormat.ts";

const normalize = (key) => key.toLowerCase().replaceAll("mod", "ctrl").split(/[+-]/).sort().join("+");

for (const [id, fields] of Object.entries({ insertSelect: ["columns", "table"], insertInsert: ["table", "columns", "values"], insertUpdate: ["table", "column", "value", "condition"] })) {
  test(`${id} expands and keeps every editable field reachable in both directions`, () => {
    let state = EditorState.create();
    const editor = { get state() { return state; }, dispatch(transaction) { state = transaction.state; } };
    const selected = () => state.sliceDoc(state.selection.main.from, state.selection.main.to);
    snippet(EDITOR_SNIPPETS[id])(editor, null, 0, 0);
    assert.equal(selected(), fields[0]);
    for (let i = 1; i < fields.length; i++) { assert.equal(nextSnippetField(editor), true); assert.equal(selected(), fields[i]); }
    // The last editable field must still let Shift+Tab return to its predecessor.
    assert.equal(prevSnippetField(editor), true);
    assert.equal(selected(), fields.at(-2));
    assert.equal(nextSnippetField(editor), true);
    assert.equal(selected(), fields.at(-1));
    assert.equal(nextSnippetField(editor), true);
    assert.equal(state.selection.main.empty, true);
    assert.equal(state.selection.main.head, state.doc.length);
    assert.equal(prevSnippetField(editor), false);
  });
}
test("editor shortcuts are unique and do not take an existing global shortcut", () => {
  const bindings = editorKeyBindings();
  const keys = bindings.map((binding) => normalize(binding.shortcut));
  assert.equal(new Set(keys).size, keys.length);
  const guidelines = readFileSync(new URL("../../../docs/ui-guidelines.md", import.meta.url), "utf8");
  const global = guidelines.split("| Anywhere |")[1].split("| SQL editor |")[0];
  const existing = global.match(/Ctrl(?:\+(?:Shift|Alt))?\+[^ ·|]+/g).map(normalize);
  for (const binding of bindings) assert.ok(!existing.includes(normalize(binding.shortcut)), `global shortcut conflict: ${binding.shortcut}`);
  for (const binding of bindings) assert.equal(normalize(binding.key), normalize(binding.shortcut));
});

test("the editor's keymap binds every editor command with keys but the clipboard, which the editor keeps (SDB-447)", () => {
  for (const command of COMMANDS.filter((command) => command.scope === "editor")) {
    if (command.fixedKeys) assert.equal(editorKeyTarget(command), null, command.id);
    else assert.ok(editorKeyTarget(command), `${command.id} has nothing to run in the editor's keymap`);
  }
  assert.equal(editorKey("Ctrl+Shift+Enter"), "Mod-Shift-Enter");
  assert.equal(editorKey("Shift+Alt+F"), "Shift-Alt-f");
  assert.equal(editorKey("Ctrl+-"), "Mod--");
  assert.equal(editorKey("Ctrl+Space"), "Mod-Space");
});

test("quoted lines preserve empty strings, Unicode, apostrophes and all line endings", () => {
  assert.equal(quotedLines("O'Reilly\r\n\r漢字\n"), "'O''Reilly', '', '漢字', ''");
});

test("quoted lists follow MySQL escapes and SQL Server Unicode literals", () => {
  for (const engine of ["mysql", "mariadb"]) assert.equal(quotedLines("a\\b'", engine), "'a\\\\b'''");
  assert.equal(quotedLines("漢字\nO'Reilly", "sqlserver"), "N'漢字', N'O''Reilly'");
  for (const engine of ["postgres", "sqlite", "duckdb"]) assert.equal(quotedLines("漢字\\path'", engine), "'漢字\\path'''");
});

for (const engine of ["postgres", "mysql", "mariadb", "sqlite", "duckdb", "sqlserver"]) {
  test(`leading comma layout preserves quoted content and comments in ${engine}`, () => {
    const input = `SELECT\n  a,\n  'quoted,\nstring',\n  "identifier,\nname",\n  b -- comment,\nFROM t`;
    const result = leadingCommas(input, engine);
    assert.equal(result, `SELECT\n  a\n  , 'quoted,\nstring'\n  , "identifier,\nname"\n  , b -- comment,\nFROM t`);
    assert.equal(leadingCommas("SELECT a, /* comment,\n here */ b", engine), "SELECT a, /* comment,\n here */ b");
  });
}
