import assert from "node:assert/strict";
import { test } from "node:test";
import { EditorSelection, EditorState } from "@codemirror/state";
import { occurrenceOrigin, selectNextOccurrence, selectAllOccurrences } from "../src/editorOccurrences.ts";

const text = ["drop table nm_events_lottery_paradise;", "drop table nm_events_lottery_paradise_stats;", "drop table nm_events_lottery_paradise_storage;", "drop table nm_events_lottery_paradise;"].join("\n");
const term = "nm_events_lottery_paradise";
function editor(selection) {
  let state = EditorState.create({ doc: text, selection, extensions: [EditorState.allowMultipleSelections.of(true), occurrenceOrigin] });
  return { get state() { return state; }, dispatch(spec) { state = state.update(spec).state; } };
}
const positions = (view) => view.state.selection.ranges.map((r) => r.from);

test("an explicit selection matches inside longer names in order and wraps", () => {
  const first = text.indexOf(term), second = text.indexOf(term, first + 1), third = text.indexOf(term, second + 1), fourth = text.indexOf(term, third + 1);
  const view = editor(EditorSelection.single(first, first + term.length));
  assert.equal(selectNextOccurrence(view), true); assert.deepEqual(positions(view), [first, second]);
  assert.equal(selectNextOccurrence(view), true); assert.deepEqual(positions(view), [first, second, third]);
  assert.equal(selectNextOccurrence(view), true); assert.deepEqual(positions(view), [first, second, third, fourth]);
  assert.equal(selectNextOccurrence(view), false);
  const wrapped = editor(EditorSelection.single(fourth, fourth + term.length)); selectNextOccurrence(wrapped);
  assert.deepEqual(positions(wrapped), [first, fourth]);
});

test("a word selected from a cursor keeps whole-word matching for next and all", () => {
  const first = text.indexOf(term), last = text.lastIndexOf(term);
  const view = editor(EditorSelection.single(first + 2));
  assert.equal(selectNextOccurrence(view), true); assert.deepEqual(positions(view), [first]);
  assert.equal(selectNextOccurrence(view), true); assert.deepEqual(positions(view), [first, last]);
  assert.equal(selectAllOccurrences(view), true); assert.deepEqual(positions(view), [first, last]);
});

test("select all uses explicit text as a substring and manual selection resets word mode", () => {
  const first = text.indexOf(term), view = editor(EditorSelection.single(first + 1));
  selectNextOccurrence(view);
  view.dispatch({ selection: EditorSelection.single(first, first + term.length) });
  assert.equal(selectAllOccurrences(view), true); assert.equal(view.state.selection.ranges.length, 4);
});

test("typing replaces only the selected occurrences", () => {
  const first = text.indexOf(term), view = editor(EditorSelection.single(first, first + term.length));
  selectNextOccurrence(view);
  view.dispatch(view.state.changeByRange((range) => ({
    changes: { from: range.from, to: range.to, insert: "renamed" },
    range: EditorSelection.cursor(range.from + "renamed".length),
  })));
  assert.match(view.state.doc.toString(), /^drop table renamed;\ndrop table renamed_stats;/);
  assert.ok(view.state.doc.toString().includes(`${term}_storage`));
});
