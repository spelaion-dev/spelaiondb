// Duplicate, copy and paste of connections (ADR-030). The core makes every copy and checks
// every paste; this only asks the trust questions first and shows what happened. No password
// ever passes through here or the clipboard: the clipboard text has none.

import { call, commands, type ConnectionItem } from "./api";
import { readClipboardText } from "./clipboardText";
import { fieldList } from "./fieldNames";
import i18n from "./i18n";
import { refreshConnections } from "./components/Sidebar";
import { useApp, useDialogs } from "./store";

/**
 * Asks what opening a connection's editor asks: trust for an untrusted repository, a review
 * for a repository connection that changed since it was trusted. False if the user refused.
 */
export async function ensureEditorAccess(connectionId: string): Promise<boolean> {
  let access = await call(commands.connectionEditorAccess(connectionId));
  if (access.kind === "untrusted") {
    const shown = useApp.getState().workspace?.workspace ?? null;
    if (!(await useDialogs.getState().askTrust(shown))) return false;
    useApp.getState().setWorkspace(await call(commands.trustWorkspace(shown ?? "")));
    access = await call(commands.connectionEditorAccess(connectionId));
  }
  if (access.kind === "changed") {
    const accepted = await useDialogs.getState().askChange({ item: access.item, isNew: access.new, fields: access.fields });
    if (!accepted) return false;
    await call(commands.acceptChange(access.root, access.item, access.new, access.fields));
    access = await call(commands.connectionEditorAccess(connectionId));
  }
  return access.kind === "ready";
}

/** Shows a new connection in the explorer and opens its editor with a note. */
async function showNew(id: string, notice: string) {
  await refreshConnections();
  const app = useApp.getState();
  app.revealInExplorer({ connectionId: id });
  app.setConnectionDialog({ kind: "edit", id, notice });
}

/** "Duplicate": the copy is made by the core, next to the original, and opened to edit. */
export async function duplicateConnection(connection: ConnectionItem): Promise<void> {
  if (!(await ensureEditorAccess(connection.id))) return;
  const copy = await call(commands.duplicateConnection(connection.id));
  const notice = i18n.t(copy.passwordCopied ? "connections:copies.duplicatedWithPassword" : "connections:copies.duplicated", {
    name: connection.name,
    copy: copy.name,
  });
  await showNew(copy.id, notice);
}

/** Ctrl+C on a connection: its text without secrets goes to the clipboard. */
export async function copyConnection(connection: ConnectionItem): Promise<string> {
  if (!(await ensureEditorAccess(connection.id))) return "";
  const copied = await call(commands.copyConnection(connection.id));
  await navigator.clipboard.writeText(copied.text);
  return copied.leftOut.length > 0
    ? i18n.t("connections:copies.copiedWithout", { name: connection.name, fields: fieldList(copied.leftOut) })
    : i18n.t("connections:copies.copied", { name: connection.name });
}

/** Ctrl+V in the explorer: the core checks the text as untrusted and makes a personal copy. */
export async function pasteConnection(text: string): Promise<void> {
  const pasted = await call(commands.pasteConnection(text));
  const notice =
    pasted.leftOut.length > 0
      ? i18n.t("connections:copies.pastedWithout", { name: pasted.name, fields: fieldList(pasted.leftOut) })
      : i18n.t("connections:copies.pasted", { name: pasted.name });
  await showNew(pasted.id, notice);
}

/** "Paste" from a menu: reads the clipboard (Ctrl+V gets the text from its paste event). */
export async function pasteFromClipboard(): Promise<void> {
  let text: string;
  try {
    text = await readClipboardText();
  } catch {
    throw new Error(i18n.t("connections:copies.clipboardUnavailable"));
  }
  await pasteConnection(text);
}
