// Where a new note of a saved ER diagram goes (ADR-084, `src/erNotes.ts`): next to the focused
// table, never over a table, a note or a group.
import assert from "node:assert/strict";
import test from "node:test";
import { NEW_NOTE_HEIGHT, NOTE_GAP, NOTE_WIDTH, placeNewNote } from "../src/erNotes.ts";

const box = (x, y, width, height, extra = {}) => ({ x, y, width, height, ...extra });

function layout({ nodes = [], gone = [], notes = [], groups = [] }) {
  const all = [...nodes, ...gone, ...notes, ...groups];
  const x = Math.min(0, ...all.map((b) => b.x));
  const y = Math.min(0, ...all.map((b) => b.y));
  const right = Math.max(0, ...all.map((b) => b.x + b.width));
  const bottom = Math.max(0, ...all.map((b) => b.y + b.height));
  return { x, y, width: right - x, height: bottom - y, nodes, gone, notes, groups };
}

function overlaps(a, b) {
  return a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height;
}

function noteAt(spot) {
  return box(spot.x, spot.y, NOTE_WIDTH, NEW_NOTE_HEIGHT);
}

test("a new note goes to the right of the focused table, not on it", () => {
  const orders = box(400, 100, 200, 110, { focus: true });
  const customer = box(0, 300, 180, 110, { focus: false });
  const spot = placeNewNote(layout({ nodes: [orders, customer] }), { x: 500, y: 150 });
  assert.deepEqual(spot, { x: 400 + 200 + NOTE_GAP, y: 100 });
  assert.ok(!overlaps(noteAt(spot), orders));
});

test("a side that is taken is passed over for the next one", () => {
  const orders = box(400, 100, 200, 110, { focus: true });
  const right = box(400 + 200 + NOTE_GAP, 100, 200, 110, { focus: false });
  const spot = placeNewNote(layout({ nodes: [orders, right] }), { x: 500, y: 150 });
  assert.deepEqual(spot, { x: 400 - NOTE_GAP - NOTE_WIDTH, y: 100 });
});

test("notes and group frames are avoided too", () => {
  const orders = box(400, 100, 200, 110, { focus: true });
  const note = box(400 + 200 + NOTE_GAP, 100, NOTE_WIDTH, 60);
  const group = box(0, 0, 400 - NOTE_GAP, 400);
  const drawn = layout({ nodes: [orders], notes: [note], groups: [group] });
  const spot = placeNewNote(drawn, { x: 500, y: 150 });
  for (const other of [orders, note, group]) assert.ok(!overlaps(noteAt(spot), other), JSON.stringify({ spot, other }));
  assert.deepEqual(spot, { x: 400, y: 100 + 110 + NOTE_GAP });
});

test("without a focused table, the middle of the screen when it is empty", () => {
  const a = box(0, 0, 200, 110, { focus: false });
  const spot = placeNewNote(layout({ nodes: [a] }), { x: 1000, y: 500 });
  assert.deepEqual(spot, { x: 1000 - NOTE_WIDTH / 2, y: 500 - NEW_NOTE_HEIGHT / 2 });
});

test("when every side is taken, it goes right of everything", () => {
  const nodes = [];
  for (let col = 0; col < 6; col++) {
    for (let row = 0; row < 6; row++) {
      nodes.push(box(col * 210, row * 120, 200, 110, { focus: col === 2 && row === 2 }));
    }
  }
  const drawn = layout({ nodes });
  const spot = placeNewNote(drawn, { x: 500, y: 300 });
  for (const other of nodes) assert.ok(!overlaps(noteAt(spot), other), JSON.stringify({ spot, other }));
});
