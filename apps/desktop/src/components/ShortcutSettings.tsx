// Settings › Keyboard shortcuts (SDB-447, ADR-064): every app command of the registry with its
// keys, grouped by where the keys act and by category, found by its name or by its keys, and
// changed here. Commands without keys are listed too, so one can be given. The names and the
// keys come from the registry and are put in words by `shortcuts.ts`, as in the palette and
// the menus; a change is the user's personal preference, saved by the core as it is made.

import { useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { useTranslation } from "react-i18next";
import { commands } from "../api";
import {
  COMMAND_CATEGORIES,
  COMMANDS,
  SHORTCUT_PLACES,
  commandKeys,
  keysChanged,
  pressedShortcut,
  shortcutClash,
  shortcutMatches,
  shortcutOverrides,
  shortcutPlace,
  withKeys,
  type CommandCategory,
  type CommandSpec,
  type ShortcutChange,
  type ShortcutPlace,
} from "../commandRegistry";
import { useDialogs } from "../dialogStore";
import { useShortcuts } from "../shortcutPreferences";
import { keyText, shortcutText } from "../shortcuts";

interface Row {
  command: CommandSpec;
  name: string;
  /** The keys in words; "" for none. */
  keys: string;
}

interface Group {
  place: ShortcutPlace;
  categories: { category: CommandCategory; rows: Row[] }[];
}

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function ShortcutSettings() {
  const { t } = useTranslation("settings");
  const { t: ta } = useTranslation("app");
  const [query, setQuery] = useState("");
  const [changing, setChanging] = useState<CommandSpec | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  // Re-read on every change the core saved.
  const revision = useShortcuts((s) => s.revision);
  const loadError = useShortcuts((s) => s.loadError);

  const groups = useMemo<Group[]>(() => {
    const all = COMMANDS as readonly CommandSpec[];
    return SHORTCUT_PLACES.map((place) => ({
      place,
      categories: COMMAND_CATEGORIES.map((category) => ({
        category,
        rows: all
          .filter((command) => shortcutPlace(command) === place && command.category === category)
          .map((command) => {
            const keys = commandKeys(command);
            return { command, name: t(command.label), keys: keys ? shortcutText(keys) : "", written: keys ?? "" };
          })
          .filter((row) => shortcutMatches(query, [row.name, row.keys, row.written]))
          .map(({ command, name, keys }) => ({ command, name, keys })),
      })).filter((entry) => entry.rows.length > 0),
    })).filter((group) => group.categories.length > 0);
    // `revision` is what changes when the effective keys do.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [query, t, revision]);

  /** Saves `changes` in place of the current ones; says why when the core refuses them. */
  const save = async (changes: ShortcutChange[]) => {
    setBusy(true);
    try {
      await useShortcuts.getState().save(changes);
      setFailure(null);
    } catch (e) {
      setFailure(message(e));
    } finally {
      setBusy(false);
    }
  };
  const resetAll = async () => {
    const answer = await useDialogs.getState().askChoice({
      title: t("shortcuts.resetAllTitle"),
      message: t("shortcuts.resetAllBody", { count: shortcutOverrides().length }),
      choices: [
        { id: "cancel", label: t("shortcuts.cancel") },
        { id: "reset", label: t("shortcuts.resetAll"), primary: true },
      ],
    });
    if (answer === "reset") await save([]);
  };

  return (
    <div className="shortcut-settings">
      <p>{t("shortcuts.hint", { example: shortcutText("Ctrl+Enter") })}</p>
      <div className="shortcut-toolbar">
        <input
          type="search"
          className="shortcut-search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          placeholder={t("shortcuts.search")}
          aria-label={t("shortcuts.searchLabel")}
        />
        <button disabled={busy || shortcutOverrides().length === 0} onClick={() => void resetAll()}>
          {t("shortcuts.resetAll")}
        </button>
      </div>
      {loadError && <p className="error small" role="alert">{t("shortcuts.loadError", { detail: loadError })}</p>}
      {failure && <p className="error small" role="alert">{failure}</p>}
      {groups.length === 0 && <p className="muted" role="status">{t("shortcuts.noMatch")}</p>}
      {groups.map((group) => (
        <section key={group.place} aria-labelledby={`shortcuts-${group.place}`}>
          <h3 id={`shortcuts-${group.place}`}>{t(`shortcuts.places.${group.place}`)}</h3>
          <table className="shortcut-table">
            {group.categories.map(({ category, rows }) => (
              <tbody key={category}>
                <tr>
                  <th scope="colgroup" colSpan={3}>{ta(`commands.categories.${category}`)}</th>
                </tr>
                {rows.map((row) => {
                  const command = row.command;
                  const changed = keysChanged(command);
                  return (
                    <tr key={command.id} data-command={command.id}>
                      <td>
                        {row.name}
                        {changed && <span className="tag shortcut-changed">{t("shortcuts.changed")}</span>}
                      </td>
                      <td className="shortcut-keys">{row.keys}</td>
                      <td className="shortcut-actions">
                        {command.fixedKeys ? (
                          <span className="muted small">{t("shortcuts.fixed")}</span>
                        ) : (
                          <>
                            <button className="ghost" disabled={busy} onClick={() => setChanging(command)} aria-label={t("shortcuts.changeFor", { name: row.name })}>
                              {t("shortcuts.change")}
                            </button>
                            {row.keys && (
                              <button className="ghost" disabled={busy} onClick={() => void save(withKeys(shortcutOverrides(), command, null))} aria-label={t("shortcuts.removeFor", { name: row.name })}>
                                {t("shortcuts.remove")}
                              </button>
                            )}
                            {changed && (
                              <button className="ghost" disabled={busy} onClick={() => void save(withKeys(shortcutOverrides(), command, command.shortcut ?? null))} aria-label={t("shortcuts.resetFor", { name: row.name })}>
                                {t("shortcuts.reset")}
                              </button>
                            )}
                          </>
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            ))}
          </table>
        </section>
      ))}
      {changing && <KeysDialog command={changing} name={t(changing.label)} onClose={() => setChanging(null)} />}
    </div>
  );
}

/** What the "Press the new keys" dialog holds; only the page has it (ADR-060). */
type Capture =
  | { kind: "waiting" }
  | { kind: "checking"; keys: string }
  | { kind: "refused"; keys: string; reason: string }
  | { kind: "clash"; keys: string; other: CommandSpec }
  | { kind: "ready"; keys: string }
  | { kind: "saving"; keys: string; move: CommandSpec | null }
  | { kind: "failed"; keys: string; move: CommandSpec | null; message: string };

/** Takes a command's new keys as they are pressed, says why they cannot be its keys or which
 *  command has them, and saves them (moving them from that command when the user asks). */
function KeysDialog({ command, name, onClose }: { command: CommandSpec; name: string; onClose(): void }) {
  const { t } = useTranslation("settings");
  const [capture, setCapture] = useState<Capture>({ kind: "waiting" });
  // Only the last press is answered: the core's check is asynchronous.
  const latest = useRef(0);
  // The keys are taken where the focus starts: the box that shows them.
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => box.current?.focus(), []);

  const pressed = async (keys: string) => {
    const press = ++latest.current;
    setCapture({ kind: "checking", keys });
    const reason: string | null = await commands.checkShortcut(keys).catch((e: unknown) => message(e));
    if (press !== latest.current) return;
    if (reason) {
      setCapture({ kind: "refused", keys, reason });
      return;
    }
    const clash = shortcutClash(command, keys);
    if (clash === null) {
      setCapture({ kind: "ready", keys });
      return;
    }
    switch (clash.kind) {
      case "command":
        setCapture({ kind: "clash", keys, other: clash.command });
        return;
      case "fixed":
        setCapture({ kind: "refused", keys, reason: t("shortcuts.takenFixed", { name: t(clash.command.label) }) });
        return;
      case "region":
        setCapture({ kind: "refused", keys, reason: t("shortcuts.takenRegion", { where: t(`shortcuts.places.${clash.place}`) }) });
        return;
      default: {
        const never: never = clash;
        return never;
      }
    }
  };

  const save = async (keys: string, move: CommandSpec | null) => {
    setCapture({ kind: "saving", keys, move });
    let changes = withKeys(shortcutOverrides(), command, keys);
    if (move) changes = withKeys(changes, move, null);
    try {
      await useShortcuts.getState().save(changes);
      onClose();
    } catch (e) {
      setCapture({ kind: "failed", keys, move, message: message(e) });
    }
  };

  const saving = capture.kind === "saving";
  const onKeyDown = (event: KeyboardEvent<HTMLDivElement>) => {
    // The keys pressed here are taken, never acted on behind the dialog.
    event.stopPropagation();
    if (saving) {
      event.preventDefault();
      return;
    }
    const target = event.target as HTMLElement;
    const onButton = target.tagName === "BUTTON";
    if (event.key === "Escape" && !event.ctrlKey && !event.altKey && !event.metaKey && !event.shiftKey) {
      event.preventDefault();
      onClose();
      return;
    }
    const plain = !event.ctrlKey && !event.altKey && !event.metaKey;
    // Tab moves between the buttons; Enter or Space on one presses it.
    if (event.key === "Tab" && plain) return;
    if (onButton && (event.key === "Enter" || event.key === " ") && plain) return;
    event.preventDefault();
    // Enter alone is never a shortcut: it saves what was pressed, when it can be saved.
    if (event.key === "Enter" && plain && !event.shiftKey && (capture.kind === "ready" || capture.kind === "clash")) {
      void save(capture.keys, capture.kind === "clash" ? capture.other : null);
      return;
    }
    const keys = pressedShortcut(event.nativeEvent);
    if (keys) void pressed(keys);
  };

  const keys = capture.kind === "waiting" ? null : capture.keys;
  return (
    <div className="modal-backdrop" onKeyDown={onKeyDown}>
      <div className="modal shortcut-dialog" role="dialog" aria-modal="true" aria-labelledby="shortcut-dialog-title" aria-describedby="shortcut-dialog-state">
        <h2 id="shortcut-dialog-title">{t("shortcuts.dialogTitle", { name })}</h2>
        <p>{t("shortcuts.press", { cancel: keyText("close") })}</p>
        <div className="shortcut-pressed" tabIndex={0} aria-live="polite" aria-label={t("shortcuts.pressedLabel")} ref={box}>
          {keys ? shortcutText(keys) : <span className="muted">{t("shortcuts.waiting")}</span>}
        </div>
        <div id="shortcut-dialog-state">
          {capture.kind === "refused" && <p className="error" role="alert">{capture.reason}</p>}
          {capture.kind === "clash" && (
            <p role="status">{t("shortcuts.clash", { keys: shortcutText(capture.keys), other: t(capture.other.label), name })}</p>
          )}
          {capture.kind === "failed" && <p className="error" role="alert">{capture.message}</p>}
        </div>
        <div className="dialog-actions">
          <button type="button" onClick={onClose} disabled={saving}>{t("shortcuts.cancel")}</button>
          {capture.kind === "clash" ? (
            <button type="button" className="primary" onClick={() => void save(capture.keys, capture.other)}>{t("shortcuts.move")}</button>
          ) : (
            <button
              type="button"
              className="primary"
              disabled={!(capture.kind === "ready" || capture.kind === "failed")}
              onClick={() => {
                if (capture.kind === "ready") void save(capture.keys, null);
                else if (capture.kind === "failed") void save(capture.keys, capture.move);
              }}
            >
              {t(saving ? "shortcuts.saving" : "shortcuts.save")}
            </button>
          )}
        </div>
      </div>
    </div>
  );
}
