// Matches of the grid's search (Ctrl+F) for the rows on screen, asked of the core a block at
// a time. The search itself runs in the core over the whole result (ADR-010): only the
// positions of the cells to highlight come here.

import { call, commands } from "../api";

const BLOCK = 500;
const MAX_BLOCKS = 40;

export class FindCache {
  private blocks = new Map<number, Set<number>>();
  private pending = new Set<number>();

  constructor(
    readonly resultId: number,
    private readonly columns: number,
    private readonly onLoaded: () => void,
  ) {}

  /** Whether the cell matches; `undefined` while its block is on its way. */
  has(row: number, col: number): boolean | undefined {
    const block = this.blocks.get(Math.floor(row / BLOCK));
    return block ? block.has(row * this.columns + col) : undefined;
  }

  /** Asks for the blocks of rows `[first, last)` that have not arrived. */
  load(first: number, last: number) {
    for (let b = Math.floor(first / BLOCK); b * BLOCK < last; b++) {
      if (this.blocks.has(b) || this.pending.has(b)) continue;
      this.pending.add(b);
      call(commands.findMatches(this.resultId, b * BLOCK, BLOCK))
        .then((matches) => {
          this.blocks.set(b, new Set(matches.map((m) => m.row * this.columns + m.column)));
          while (this.blocks.size > MAX_BLOCKS) {
            const oldest = this.blocks.keys().next().value;
            if (oldest === undefined) break;
            this.blocks.delete(oldest);
          }
          this.onLoaded();
        })
        .catch(() => undefined)
        .finally(() => this.pending.delete(b));
    }
  }
}
