# ADR-064: Keyboard shortcuts are a personal preference: the user's differences by command, checked by the core, judged by where keys act

- **Status:** Accepted
- **Date:** 2026-11-22
- **Builds on:** SDB-18 (the command registry), SDB-86 (one table that writes every shortcut),
  ADR-060 (states the page owns are discriminated unions)

## Context
Pablo asked for a place in Settings that lists the keyboard shortcuts and lets the user change
them (SDB-447). Before this, every shortcut was a constant in `commandRegistry.ts`. Since SDB-86,
everything that *shows* a shortcut read that table through `shortcuts.ts`. The keys that *acted*
were not all read from it: the SQL editor's CodeMirror keymap wrote its keys again by hand, and
the grid read its commands' defaults once when it loaded.

Changing a shortcut needs four things decided: whose choice it is, what is stored and where,
which keys can never be taken, and when two commands' keys clash.

## Decision
1. **Shortcuts are a personal preference, never the project's or the workspace's.** They are kept
   with the other personal settings, in the user directory's `state.db` (`spelaion-store`'s
   `shortcuts.rs`, one `personal_settings` row per changed command). Every workspace of that user
   shares them, and nothing about them is ever committed or shared with a team. The page applies
   and saves each change at once, with no Save button, like the other personal preferences.
2. **Only the differences from the defaults are stored:** a command's id with the keys the user
   gave it, or with none. The registry keeps the defaults, so a default we change later still
   reaches everyone who did not touch that command. A stored id that is no longer a command is
   ignored, and so are keys equal to the command's default.
3. **The core decides which keys a command may take** (`spelaion-core`'s `shortcuts.rs`), when
   saving and again when reading. The keys must be written as the registry writes them:
   modifiers in the order Ctrl, Shift, Alt, then one key, with letters in capitals. The core
   refuses, and the dialog says why:
   - keys that type or edit text, meaning no Ctrl or Alt with a character, the space bar,
     Backspace, Delete, the arrows, Home, End, PageUp or PageDown;
   - Escape, Enter and Tab alone or with Shift, which dialogs and the editor keep;
   - Ctrl+Alt with a character that is not a letter or a digit. On Windows Ctrl+Alt is AltGr,
     so the keys are matched by `KeyboardEvent.key` as today, and AltGr's character is what
     arrives;
   - keys the system or the webview keeps: Alt+F4, Alt+Tab, Alt+Space, Ctrl+Escape and the like.

   The set is saved whole, or not at all.

   **No user's keys can stand in for a confirmation's answer.** The keys a dialog answers with
   (Enter, Escape, Tab between its buttons, and 1–9 in the statement picker) are refused above.
   A dialog handles its keys on its own element before the global handler sees them. The global
   handler runs no command while a modal dialog is open, and the SQL editor's keymap acts only
   while the editor has the focus. So a destructive confirmation answers as it always did,
   whatever keys the user gave the commands.
4. **Some keys cannot be changed:** a region's own keys (`KEYS` in the registry: dialogs, the
   grid's selection, copy and find keys, the edits' keys, the explorer's keys, the editor's Ctrl+D
   and snippet keys), the clipboard commands whose keys the editor itself handles (`fixedKeys`),
   text inputs' keys and CodeMirror's defaults. Settings lists the fixed commands as "Kept by the
   editor".
5. **A clash is judged by where the keys act** (`shortcutClash` in the registry, judged by the page
   because the registry is the page's):
   - Two commands clash when they act in the same place (Anywhere, the SQL editor, the result
     grid), or when one of them acts anywhere.
   - The SQL editor and the grid are focused apart, so the same keys in both do not clash.
   - A region's own keys clash with a command of their place, and the keys of anywhere (Ctrl+Tab)
     with every command.
   - A command's own default never clashes, so Reset always works.

   When another command has the keys, the dialog names it and moves the keys only when the user
   asks, which leaves that command without a shortcut. Keys a region keeps are refused, naming
   the place.
6. **One table acts.** The registry answers the effective keys (`commandKeys`): the user's, or the
   default. The global handler, the SQL editor's keymap (built from the registry in a compartment
   that is rebuilt on every change), the grid's commands, the menus, the palette and the tooltips
   all read it. A change re-renders whatever shows a shortcut, as a change of language does.
   `scripts/commands.test.mjs` fails if a handler or the keymap writes a command's keys by hand.

## Consequences
- A command keeps working with its default for every user who never changed it, including after
  we change that default.
- The rule for clashes is the registry's, in the page, and the core cannot check it: the core
  does not know the commands. A store edited by another program can still give two commands the
  same keys. The first one in the registry's order then wins. That is accepted, because only the
  user's own preference is affected.
- The defaults themselves must not clash under rule 5: a test checks it.
- macOS stores keys as the registry writes them (Ctrl meaning Cmd, as the handlers take either)
  and shows them through `shortcuts.ts` (⌘ ⌥ ⇧).

## Revisit if
- Plugins or other sources start to add commands: their ids and defaults would need the same
  check, and possibly a place of their own.
- Users ask for chords of two key presses (Ctrl+K Ctrl+C), which this format cannot write.
