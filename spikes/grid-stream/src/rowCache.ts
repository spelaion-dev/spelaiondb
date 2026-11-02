import { Type, type Table } from "apache-arrow";
import { fetchWindow } from "./api";

export const BLOCK = 1000;
const MAX_BLOCKS = 64;

interface Block {
  table: Table;
  /** Formatter per column, decided once per block from the Arrow type. */
  formatters: ((row: number) => string)[];
}

/**
 * Cache of row blocks requested from the backend. Only what was seen recently lives in JS
 * (at most MAX_BLOCKS * BLOCK rows); the full result is in Rust.
 */
export class RowCache {
  private blocks = new Map<number, Block>();
  private pending = new Set<number>();
  readonly fetchLatenciesMs: number[] = [];

  constructor(
    readonly resultId: number,
    private readonly onBlockLoaded: (block: number) => void,
  ) {}

  /** Cell text, or undefined if its block has not arrived yet (and requests it). */
  get(row: number, col: number): string | undefined {
    const b = Math.floor(row / BLOCK);
    const block = this.blocks.get(b);
    if (!block) {
      this.ensure(b);
      return undefined;
    }
    // LRU: reinserting moves the block to the end of the insertion order.
    this.blocks.delete(b);
    this.blocks.set(b, block);
    return block.formatters[col]?.(row - b * BLOCK) ?? "";
  }

  ensure(b: number) {
    if (this.blocks.has(b) || this.pending.has(b)) return;
    this.pending.add(b);
    const started = performance.now();
    fetchWindow(this.resultId, b * BLOCK, BLOCK)
      .then((table) => {
        this.fetchLatenciesMs.push(performance.now() - started);
        this.blocks.set(b, { table, formatters: makeFormatters(table) });
        while (this.blocks.size > MAX_BLOCKS) {
          const oldest = this.blocks.keys().next().value as number;
          this.blocks.delete(oldest);
        }
        this.onBlockLoaded(b);
      })
      .catch((e) => console.error("fetch_window failed", e))
      .finally(() => this.pending.delete(b));
  }

  /** Forgets incomplete blocks (the result keeps growing while it loads). */
  invalidateFrom(row: number) {
    const from = Math.floor(row / BLOCK);
    for (const b of [...this.blocks.keys()]) {
      if (b >= from) this.blocks.delete(b);
    }
  }
}

function makeFormatters(table: Table): ((row: number) => string)[] {
  return table.schema.fields.map((field, i) => {
    const vec = table.getChildAt(i);
    if (!vec) return () => "";
    const fmt = (v: unknown): string => {
      if (v === null || v === undefined) return "NULL";
      switch (field.type.typeId) {
        case Type.Timestamp:
          return new Date(Number(v)).toISOString().replace("T", " ").slice(0, 19);
        case Type.Float:
          return (v as number).toFixed(2);
        default:
          return String(v);
      }
    };
    return (row: number) => fmt(vec.get(row));
  });
}
