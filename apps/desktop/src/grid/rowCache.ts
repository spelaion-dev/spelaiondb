// Blocks of rows requested from the core. Only what was seen recently lives in JS; the
// full result is in Rust (ADR-010). Every cell already arrives as text: as the database
// prints it and, where the user's display settings show it otherwise (SDB-385), as the grid
// shows it. Only drawing uses the second; everything else keeps the database's text.

import type { Table, Vector } from "apache-arrow";
import { fetchWindow, windowError, type WindowErrorItem } from "../api";

export const BLOCK = 500;
const MAX_BLOCKS = 80;

/** What kind of value a column holds, as the core says it with each window. */
export type ValueKind = "number" | "text" | "date" | "time" | "timestamp" | "timestamptz" | "bool" | "binary" | "other";

/** What the core says of a column with each window: its kind and the zone it is shown in. */
export interface ColumnShown {
  kind: ValueKind;
  zone?: string;
}

interface Block {
  /** Each column as the database prints it. */
  stored: Vector[];
  /** Each column as it is shown; a cell is null where that is how the database prints it. */
  shown: Vector[];
}

export class RowCache {
  private blocks = new Map<number, Block>();
  private pending = new Set<number>();
  /** Blocks the core could not give, with its reason: not asked again at every paint (SDB-367),
   *  only when the result grows or the cache is made again. */
  private failed = new Map<number, string>();
  /** The core no longer keeps the result: nothing is asked any more. */
  private freed = false;
  private revisions = new Map<number, number>();
  private loadedRows = 0;
  private columns: ColumnShown[] = [];

  constructor(
    readonly resultId: number,
    private readonly onLoaded: () => void,
    /** A window could not be read, or the core no longer keeps the result (SDB-367). */
    private readonly onFailed: (error: WindowErrorItem) => void = () => undefined,
  ) {}

  /** Cell text as the database prints it; `null` if it is NULL; `undefined` if its block has
   *  not arrived yet, or could not be read ([`RowCache.failure`] says why). */
  get(row: number, col: number): string | null | undefined {
    const b = Math.floor(row / BLOCK);
    const block = this.blocks.get(b);
    if (!block) {
      if (!this.freed && !this.failed.has(b)) this.request(b);
      return undefined;
    }
    const value = block.stored[col]?.get(row - b * BLOCK) as string | null | undefined;
    return value ?? null;
  }

  /** Cell text as the grid draws it: the user's display format where it applies, else the
   *  database's text. For drawing and measuring only. */
  shown(row: number, col: number): string | null | undefined {
    const stored = this.get(row, col);
    if (stored === null || stored === undefined) return stored;
    const b = Math.floor(row / BLOCK);
    const shown = this.blocks.get(b)?.shown[col]?.get(row - b * BLOCK) as string | null | undefined;
    return shown ?? stored;
  }

  /** Why the row's block could not be read, if it could not. */
  failure(row: number): string | undefined {
    return this.failed.get(Math.floor(row / BLOCK));
  }

  /** What the core said of a column with the last window; `undefined` until one arrives. */
  column(col: number): ColumnShown | undefined {
    return this.columns[col];
  }

  /** The result keeps growing: the last block may have arrived incomplete. */
  grewTo(rows: number) {
    if (rows === this.loadedRows) return;
    const last = Math.floor(this.loadedRows / BLOCK);
    this.blocks.delete(last);
    this.failed.delete(last);
    this.revisions.set(last, (this.revisions.get(last) ?? 0) + 1);
    this.loadedRows = rows;
  }

  private request(b: number) {
    if (this.pending.has(b)) return;
    this.pending.add(b);
    const revision = this.revisions.get(b) ?? 0;
    fetchWindow(this.resultId, b * BLOCK, BLOCK)
      .then((table: Table) => {
        if ((this.revisions.get(b) ?? 0) !== revision) return;
        // The first half of the columns is the database's text, the second half the shown one.
        const half = table.numCols / 2;
        const stored: Vector[] = [];
        const shown: Vector[] = [];
        const columns: ColumnShown[] = [];
        for (let i = 0; i < half; i++) {
          const storedColumn = table.getChildAt(i);
          const shownColumn = table.getChildAt(half + i);
          if (!storedColumn || !shownColumn) continue;
          stored.push(storedColumn);
          shown.push(shownColumn);
          const metadata = table.schema.fields[half + i]?.metadata;
          const zone = metadata?.get("spelaion.zone");
          columns.push({ kind: (metadata?.get("spelaion.kind") ?? "other") as ValueKind, ...(zone ? { zone } : {}) });
        }
        this.columns = columns;
        this.blocks.set(b, { stored, shown });
        while (this.blocks.size > MAX_BLOCKS) {
          const oldest = this.blocks.keys().next().value;
          if (oldest === undefined) break;
          this.blocks.delete(oldest);
        }
        this.onLoaded();
      })
      .catch((e: unknown) => {
        if ((this.revisions.get(b) ?? 0) !== revision) return;
        const error = windowError(e);
        switch (error.kind) {
          case "freed":
            this.freed = true;
            break;
          case "failed":
            this.failed.set(b, error.message);
            this.onLoaded();
            break;
          default: {
            const unknown: never = error;
            return unknown;
          }
        }
        this.onFailed(error);
      })
      .finally(() => {
        this.pending.delete(b);
        if ((this.revisions.get(b) ?? 0) !== revision) this.onLoaded();
      });
  }
}
