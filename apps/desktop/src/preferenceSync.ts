// Personal preferences follow every window (SDB-533, SDB-535). A window reads them once when it
// opens; whenever any window saves one, or a settings file is imported, the core says which
// changed and each window reads those again. A window opened earlier would otherwise keep the
// old value and save it back with its next change.

import { events } from "./api";
import type { PreferenceKind } from "./bindings";
import { useDisplayFormat } from "./displayFormat";
import { usePreferences } from "./preferences";
import { useShortcuts } from "./shortcutPreferences";
import { useSnippets } from "./snippetStore";
import { useUpdate } from "./update";

let following = false;

/** Reads again, from the core, what the core says changed. Once per window. */
export function followPreferences(): void {
  if (following) return;
  following = true;
  void events.preferencesChanged.listen((event) => {
    for (const kind of new Set(event.payload.settings)) void readAgain(kind);
  });
}

function readAgain(kind: PreferenceKind): Promise<void> {
  switch (kind) {
    case "language":
    case "theme":
    case "interfaceSize":
    case "explorer":
    case "grid":
    case "script":
    case "editor":
    case "map":
    case "reconnectOnStart":
      return usePreferences.getState().changedElsewhere(kind);
    case "displayFormat":
      return useDisplayFormat.getState().changedElsewhere();
    case "shortcuts":
      return useShortcuts.getState().load();
    case "snippets":
      return useSnippets.getState().load();
    case "updateCheck":
      return useUpdate.getState().readCheckAtStartup();
    default: {
      const unknown: never = kind;
      return unknown;
    }
  }
}
