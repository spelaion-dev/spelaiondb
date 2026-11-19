// How a result ended, as the core says it (SDB-367): the page only reads the generated type and
// words it; it never works it out from the statement's messages. Kept free of React so
// `scripts/run-results.test.mjs` can check it.

import type { PartialReasonItem, ResultEndItem } from "./bindings";

/** No more rows will arrive: the result is whole or was cut short. */
export function hasEnded(end: ResultEndItem): boolean {
  switch (end.kind) {
    case "loading":
      return false;
    case "whole":
    case "partial":
      return true;
  }
}

/** There is a result and no more of its rows will arrive. */
export function ended(result: { end: ResultEndItem } | null | undefined): boolean {
  return !!result && hasEnded(result.end);
}

/** Why the result was cut short, or `null` when it was not. */
export function partialReason(end: ResultEndItem): PartialReasonItem | null {
  switch (end.kind) {
    case "loading":
    case "whole":
      return null;
    case "partial":
      return end.reason;
  }
}

/** The translation key (in `editor`) of a partial result's strip, counted by its rows. */
export function partialKey(reason: PartialReasonItem): string {
  switch (reason) {
    case "cancelled":
      return "results.partial.cancelled";
    case "failed":
      return "results.partial.failed";
    case "timedOut":
      return "results.partial.timedOut";
    case "connectionLost":
      return "results.partial.connectionLost";
  }
}

type Translate = (key: string, values: Record<string, unknown>) => string;

/** "Partial: cancelled after 120,000 rows", or `null` for a result that is not partial. */
export function partialText(end: ResultEndItem, rows: number, t: Translate, numbers: Intl.NumberFormat): string | null {
  const reason = partialReason(end);
  return reason ? t(partialKey(reason), { count: rows, formattedCount: numbers.format(rows) }) : null;
}
