// Where a new note of a saved ER diagram goes (ADR-084): a page-only choice, in the layout's
// coordinates. The core keeps the note where the user leaves it.

/** A new note's width, in the layout's pixels (the core allows 120 to 800). */
export const NOTE_WIDTH = 240;
/** The room a new note is given before its text is known (a few lines). */
export const NEW_NOTE_HEIGHT = 72;
/** The space kept between a new note and what is drawn. */
export const NOTE_GAP = 24;

export type Box = { x: number; y: number; width: number; height: number };

/** What a new note must not cover: the layout's tables (the focused one marked), gone tables,
 *  notes and group frames, and the layout's own bounds. */
export type DrawnBoxes = Box & {
  nodes: (Box & { focus: boolean })[];
  gone: Box[];
  notes: Box[];
  groups: Box[];
};

/** Where a new note goes: in empty space next to the focused table (right, left, below,
 *  above), else in the middle of the screen (`middle`, in the layout's coordinates), else next
 *  to the table nearest that middle, else to the right of everything. It never covers a table,
 *  a note or a group. */
export function placeNewNote(layout: DrawnBoxes, middle: { x: number; y: number }): { x: number; y: number } {
  const taken: Box[] = [...layout.nodes, ...layout.gone, ...layout.notes, ...layout.groups];
  const half = NOTE_GAP / 2;
  const free = (b: Box) =>
    !taken.some((t) => b.x < t.x + t.width + half && t.x < b.x + b.width + half && b.y < t.y + t.height + half && t.y < b.y + b.height + half);
  const note = (x: number, y: number): Box => ({ x: Math.round(x), y: Math.round(y), width: NOTE_WIDTH, height: NEW_NOTE_HEIGHT });
  const around = (t: Box): Box[] => [
    note(t.x + t.width + NOTE_GAP, t.y),
    note(t.x - NOTE_GAP - NOTE_WIDTH, t.y),
    note(t.x, t.y + t.height + NOTE_GAP),
    note(t.x, t.y - NOTE_GAP - NEW_NOTE_HEIGHT),
  ];
  const centre = note(middle.x - NOTE_WIDTH / 2, middle.y - NEW_NOTE_HEIGHT / 2);
  const distance = (t: Box) => Math.hypot(t.x + t.width / 2 - middle.x, t.y + t.height / 2 - middle.y);
  const focus = layout.nodes.filter((n) => n.focus);
  const nearest = [...layout.nodes, ...layout.gone].sort((a, b) => distance(a) - distance(b));
  const candidates = [...focus.flatMap(around), centre, ...nearest.flatMap(around)];
  const right = Math.max(layout.x + layout.width, ...taken.map((t) => t.x + t.width));
  const spot = candidates.find(free) ?? note(right + NOTE_GAP, focus[0]?.y ?? centre.y);
  return { x: spot.x, y: spot.y };
}
