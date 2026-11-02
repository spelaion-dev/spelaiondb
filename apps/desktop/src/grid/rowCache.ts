// Blocks of rows requested from the core. Only what was seen recently lives in JS; the
// full result is in Rust (ADR-010). Every cell already arrives as text.

import type { Table, Vector } from "apache-arrow";
import { fetchWindow } from "../api";

export const BLOCK = 500;
const MAX_BLOCKS = 80;

export class RowCache {
  private blocks = new Map<number, Vector[]>();
  private pending = new Set<number>();
  private loadedRows = 0;
  /** Bumped by `reset`, so blocks requested before it are dropped when they arrive. */
  private generation = 0;

  constructor(
    readonly resultId: number,
    private readonly onLoaded: () => void,
  ) {}

  /** Cell text; `null` if it is NULL; `undefined` if its block has not arrived yet. */
  get(row: number, col: number): string | null | undefined {
    const b = Math.floor(row / BLOCK);
    const block = this.blocks.get(b);
    if (!block) {
      this.request(b);
      return undefined;
    }
    const value = block[col]?.get(row - b * BLOCK) as string | null | undefined;
    return value ?? null;
  }

  /** The result keeps growing: the last block may have arrived incomplete. */
  grewTo(rows: number) {
    if (rows === this.loadedRows) return;
    const last = Math.floor(this.loadedRows / BLOCK);
    this.blocks.delete(last);
    this.loadedRows = rows;
  }

  /** The rows changed order in the core (sorting): everything is asked for again. */
  reset() {
    this.generation++;
    this.blocks.clear();
    this.pending.clear();
    this.onLoaded();
  }

  private request(b: number) {
    if (this.pending.has(b)) return;
    this.pending.add(b);
    const generation = this.generation;
    fetchWindow(this.resultId, b * BLOCK, BLOCK)
      .then((table: Table) => {
        if (generation !== this.generation) return;
        const columns: Vector[] = [];
        for (let i = 0; i < table.numCols; i++) {
          const child = table.getChildAt(i);
          if (child) columns.push(child);
        }
        this.blocks.set(b, columns);
        while (this.blocks.size > MAX_BLOCKS) {
          const oldest = this.blocks.keys().next().value;
          if (oldest === undefined) break;
          this.blocks.delete(oldest);
        }
        this.onLoaded();
      })
      .catch((e: unknown) => console.error("fetch_window", e))
      .finally(() => {
        if (generation === this.generation) this.pending.delete(b);
      });
  }
}
