// The editor's snippets (SDB-384): which snippet a typed name expands, how the user's own
// override the built-in ones, and which are offered while typing.
import assert from "node:assert/strict";
import test from "node:test";
import { EditorState } from "@codemirror/state";
import { snippet } from "@codemirror/autocomplete";
import { BUILT_IN_SNIPPETS, allSnippets, lowerKeywords, snippetBefore, snippetsStarting } from "../src/editorSnippets.ts";
import { EDITOR_SNIPPETS, SNIPPET_COMMANDS } from "../src/editorActions.ts";

const own = [
  { name: "top", description: "First rows", body: "SELECT ${1:*}\nFROM ${2:table}\nLIMIT ${3:10};${0}" },
  { name: "sel", description: "My select", body: "select ${1:*} from ${2:t};" },
  { name: "Top", description: "", body: "x" },
  { name: "cnt_rows", description: "", body: "SELECT count(*) FROM ${1:table};" },
];
const names = (snippets) => snippets.map((s) => `${s.name}${s.builtIn ? "*" : ""}`);

test("the user's snippets follow the built-in ones, and replace one by name", () => {
  assert.deepEqual(names(allSnippets([])), ["sel*", "ins*", "upd*"]);
  const all = allSnippets(own);
  assert.deepEqual(names(all), ["sel", "ins*", "upd*", "top", "Top", "cnt_rows"]);
  assert.equal(all[0].body, "select ${1:*} from ${2:t};");
  assert.equal(all[0].description, "My select");
  // The "insert snippet" commands insert the snippet of that name, the user's if there is one.
  assert.deepEqual(SNIPPET_COMMANDS, { insertSelect: "sel", insertInsert: "ins", insertUpdate: "upd" });
  assert.equal(EDITOR_SNIPPETS.insertSelect, BUILT_IN_SNIPPETS[0].body);
});

test("Tab expands the whole word before the cursor when it is a snippet's name, with its case", () => {
  const all = allSnippets(own);
  const found = (before) => {
    const hit = snippetBefore(before, all);
    return hit ? `${hit.snippet.name}@${hit.start}` : null;
  };
  assert.equal(found("top"), "top@3");
  assert.equal(found("  select 1;\ttop"), "top@3");
  assert.equal(found("(top"), "top@3");
  assert.equal(found("Top"), "Top@3");
  assert.equal(found("cnt_rows"), "cnt_rows@8");
  assert.equal(found("ins"), "ins@3");
  // Part of a longer word, after a digit or an underscore, another case, or nothing typed.
  assert.equal(found("stop"), null);
  assert.equal(found("atop"), null);
  assert.equal(found("1top"), null);
  assert.equal(found("_top"), null);
  assert.equal(found("TOP"), null);
  assert.equal(found("top "), null);
  assert.equal(found("top;"), null);
  assert.equal(found(""), null);
  assert.equal(found("rows"), null);
});

test("snippets whose name starts with the typed word are offered, the exact one first", () => {
  const all = allSnippets(own);
  assert.deepEqual(names(snippetsStarting("to", all)), ["top", "Top"]);
  assert.deepEqual(names(snippetsStarting("Top", all)), ["Top", "top"]);
  assert.deepEqual(names(snippetsStarting("s", all)), ["sel"]);
  assert.deepEqual(names(snippetsStarting("u", all)), ["upd*"]);
  assert.deepEqual(names(snippetsStarting("x", all)), []);
  assert.deepEqual(names(snippetsStarting("", all)), []);
});

test("a user snippet expands with its fields in order, as the built-in ones do", () => {
  // CodeMirror reads the user's text with the same syntax: the first field is selected, and
  // the text is inserted as written (nothing is run or changed).
  let state = EditorState.create({ doc: "" });
  const editor = { state, dispatch(tr) { state = tr.state; this.state = state; } };
  snippet(own[0].body)(editor, null, 0, 0);
  assert.equal(state.doc.toString(), "SELECT *\nFROM table\nLIMIT 10;");
  assert.equal(state.sliceDoc(state.selection.main.from, state.selection.main.to), "*");
  // Only a built-in snippet follows the keyword case preference.
  assert.equal(lowerKeywords(BUILT_IN_SNIPPETS[2].body), "update ${1:table} set ${2:column} = ${3:value} where ${4:condition};${0}");
});
