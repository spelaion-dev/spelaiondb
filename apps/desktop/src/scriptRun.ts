// What the Messages tab says about a script run as a whole (ADR-035): the core counts and
// decides; this only turns its summary into translated lines.

import type { ScriptSummaryItem } from "./api";
import type { Message, Translation } from "./store";

/** "5 statements: 4 OK, 1 failed, 0 not run (12 ms execute, 3 ms fetch)", then why it stopped. */
export function summaryMessages(summary: ScriptSummaryItem): Message[] {
  const failed = summary.failed > 0 || summary.stop != null;
  const out: Message[] = [{ kind: failed ? "error" : "info", text: "", translation: summaryTranslation(summary) }];
  if (summary.stop) {
    out.push({
      kind: "error",
      text: "",
      translation: { key: `messages.stop.${summary.stop.kind}`, values: { index: summary.stop.index + 1 } },
    });
  }
  return out;
}

export function summaryTranslation(summary: ScriptSummaryItem): Translation {
  return {
    key: "messages.summary",
    values: {
      count: summary.statements,
      ok: summary.succeeded,
      failed: summary.failed,
      notRun: summary.notRun,
      execution: Math.round(summary.executionMs ?? 0),
      fetch: Math.round(summary.fetchMs ?? 0),
    },
  };
}

/** The status line's short form, for a script of several statements. */
export function summaryShort(summary: ScriptSummaryItem | undefined): Translation | null {
  if (!summary || summary.statements < 2) return null;
  return {
    key: "results.summary",
    values: { ok: summary.succeeded, failed: summary.failed, notRun: summary.notRun },
  };
}
