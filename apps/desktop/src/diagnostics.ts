// Diagnostics for bug reports (SDB-398). The report is written and scrubbed by the core
// (`spelaion-core`'s `diagnostics.rs`): the page only copies the text it returns. Nothing is
// sent anywhere; the issue page opens only on a click, at a fixed address with no report in it.

import { call, commands } from "./api";
import type { ScreenFailureItem } from "./bindings";

/** Copies the diagnostic report, with the failed screen when there is one. */
export async function copyDiagnostics(failure: ScreenFailureItem | null = null): Promise<void> {
  const text = await call(commands.diagnosticInfo(failure));
  await navigator.clipboard.writeText(text);
}

/** Opens GitHub's new-issue page in the browser (the bug report form, empty). */
export async function openIssuePage(): Promise<void> {
  await call(commands.openIssuePage());
}
