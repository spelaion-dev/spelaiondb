// A shortcut as the reader sees it (SDB-86): its keys come from one place, the command registry
// (a command's shortcut, or `KEYS` for a region's own keys), and are put in words here, with the
// interface's names for the keys ("Mayús", "Intro") and ⌘ ⌥ ⇧ on macOS. Translated strings
// never write a shortcut: they interpolate what these return, and a tooltip appends it.
//
// Call them while rendering, so a change of language writes the keys again. A change to the
// user's shortcuts (SDB-447) re-renders the same way: `shortcutPreferences.ts` emits
// `shortcutsChanged`, which `i18n.ts` binds as it binds a change of language.

import { commandKeys, commandSpec, KEYS, shortcutLabel, type CommandId, type KeyId } from "./commandRegistry";
import i18n from "./i18n";

/** macOS, where the handlers take Cmd for Ctrl. */
export function isMac(): boolean {
  if (typeof navigator === "undefined") return false;
  return /Mac|iPhone|iPad/.test(navigator.platform || navigator.userAgent);
}

/** A key's name in the interface's language, or the key as written. */
function keyName(key: string): string {
  return i18n.t(`shell:keys.${key}`, { defaultValue: key });
}

/** A shortcut written as the registry writes it ("Ctrl+Shift+Enter"), in words for the reader. */
export function shortcutText(shortcut: string): string {
  return shortcutLabel(shortcut, { mac: isMac(), keyName });
}

/** A command's shortcut in words (the user's keys, or its default), or "" when it has none. */
export function commandShortcut(id: CommandId): string {
  const shortcut = commandKeys(commandSpec(id));
  return shortcut ? shortcutText(shortcut) : "";
}

/** One of a region's own keys (`KEYS`) in words. */
export function keyText(id: KeyId): string {
  return shortcutText(KEYS[id].shortcut);
}

/** A shortcut as `aria-keyshortcuts` writes it: "Control+Shift+C", "Meta+C" on macOS. */
export function ariaKeyShortcuts(shortcut: string): string {
  const ctrl = isMac() ? "Meta" : "Control";
  return shortcut.replace(/^Ctrl(?=\+)/, ctrl);
}

/** "Label (Shortcut)", for a tooltip; the label alone without a shortcut. */
export function withShortcut(label: string, shortcut: string): string {
  return shortcut ? `${label} (${shortcut})` : label;
}

/** A command's tooltip: its label and its shortcut. */
export function commandTitle(label: string, id: CommandId): string {
  return withShortcut(label, commandShortcut(id));
}
