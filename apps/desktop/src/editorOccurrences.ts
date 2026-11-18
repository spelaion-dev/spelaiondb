import { EditorSelection, StateEffect, StateField } from "@codemirror/state";
import { SearchCursor } from "@codemirror/search";
import type { EditorView } from "@codemirror/view";

// Only a word first selected by the shortcut keeps whole-word matching on subsequent presses.
const origin = StateEffect.define<boolean>();
export const occurrenceOrigin = StateField.define<boolean>({
  create: () => false,
  update(value, transaction) {
    for (const effect of transaction.effects) if (effect.is(origin)) return effect.value;
    return transaction.selection || transaction.docChanged ? false : value;
  },
});

function matches(view: EditorView) {
  const { state } = view;
  const selection = state.selection.main;
  const word = selection.empty ? state.wordAt(selection.head) : null;
  const target = word ?? (selection.empty ? null : selection);
  if (!target) return null;
  const text = state.sliceDoc(target.from, target.to);
  const wholeWord = !!word || state.field(occurrenceOrigin, false) === true;
  const ranges = [];
  for (const cursor = new SearchCursor(state.doc, text); !cursor.next().done;) {
    const { from, to } = cursor.value;
    if (wholeWord) {
      const found = state.wordAt(from);
      if (!found || found.from !== from || found.to !== to) continue;
    }
    ranges.push(EditorSelection.range(from, to));
  }
  return { target, ranges, wholeWord };
}

export function selectNextOccurrence(view: EditorView): boolean {
  const result = matches(view);
  if (!result) return false;
  const { state } = view;
  if (state.selection.main.empty) {
    view.dispatch({ selection: EditorSelection.create([result.target]), effects: origin.of(true), userEvent: "select.search" });
    return true;
  }
  const selected = state.selection.ranges;
  const candidates = result.ranges.filter((candidate) => !selected.some((range) => candidate.from < range.to && candidate.to > range.from));
  const next = candidates.find((candidate) => candidate.from >= state.selection.main.to) ?? candidates[0];
  if (!next) return false;
  const ranges = [...selected, next].sort((a, b) => a.from - b.from);
  view.dispatch({ selection: EditorSelection.create(ranges, ranges.indexOf(next)), effects: origin.of(result.wholeWord), scrollIntoView: true, userEvent: "select.search" });
  return true;
}

export function selectAllOccurrences(view: EditorView): boolean {
  const result = matches(view);
  if (!result || !result.ranges.length) return false;
  const main = Math.max(0, result.ranges.findIndex((range) => range.from === result.target.from && range.to === result.target.to));
  view.dispatch({ selection: EditorSelection.create(result.ranges, main), effects: origin.of(result.wholeWord), userEvent: "select.search.matches" });
  return true;
}
