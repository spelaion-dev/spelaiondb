// Settings › Keyboard shortcuts (SDB-447): every app command of the registry with its keys,
// grouped by where the keys act and by category, found by its name or by its keys. Commands
// without keys are listed too. The names and the keys come from the registry and are put in
// words by `shortcuts.ts`, as in the palette and the menus.

import { useMemo, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  COMMAND_CATEGORIES,
  COMMANDS,
  SHORTCUT_PLACES,
  shortcutMatches,
  shortcutPlace,
  type CommandCategory,
  type CommandSpec,
  type ShortcutPlace,
} from "../commandRegistry";
import { shortcutText } from "../shortcuts";

interface Row {
  id: string;
  name: string;
  /** The keys in words; "" for none. */
  keys: string;
}

interface Group {
  place: ShortcutPlace;
  categories: { category: CommandCategory; rows: Row[] }[];
}

export function ShortcutSettings() {
  const { t } = useTranslation("settings");
  const { t: ta } = useTranslation("app");
  const [query, setQuery] = useState("");

  const groups = useMemo<Group[]>(() => {
    const commands = COMMANDS as readonly CommandSpec[];
    return SHORTCUT_PLACES.map((place) => ({
      place,
      categories: COMMAND_CATEGORIES.map((category) => ({
        category,
        rows: commands
          .filter((command) => shortcutPlace(command) === place && command.category === category)
          .map((command) => {
            const name = t(command.label);
            const keys = command.shortcut ? shortcutText(command.shortcut) : "";
            return { command, row: { id: command.id, name, keys } };
          })
          .filter(({ command, row }) => shortcutMatches(query, [row.name, row.keys, command.shortcut ?? ""]))
          .map(({ row }) => row),
      })).filter((entry) => entry.rows.length > 0),
    })).filter((group) => group.categories.length > 0);
  }, [query, t]);

  return (
    <div className="shortcut-settings">
      <p>{t("shortcuts.hint", { example: shortcutText("Ctrl+Enter") })}</p>
      <input
        type="search"
        className="shortcut-search"
        value={query}
        onChange={(event) => setQuery(event.target.value)}
        placeholder={t("shortcuts.search")}
        aria-label={t("shortcuts.searchLabel")}
      />
      {groups.length === 0 && <p className="muted" role="status">{t("shortcuts.noMatch")}</p>}
      {groups.map((group) => (
        <section key={group.place} aria-labelledby={`shortcuts-${group.place}`}>
          <h3 id={`shortcuts-${group.place}`}>{t(`shortcuts.places.${group.place}`)}</h3>
          <table className="shortcut-table">
            {group.categories.map(({ category, rows }) => (
              <tbody key={category}>
                <tr>
                  <th scope="colgroup" colSpan={2}>{ta(`commands.categories.${category}`)}</th>
                </tr>
                {rows.map((row) => (
                  <tr key={row.id} data-command={row.id}>
                    <td>{row.name}</td>
                    <td className="shortcut-keys">{row.keys}</td>
                  </tr>
                ))}
              </tbody>
            ))}
          </table>
        </section>
      ))}
    </div>
  );
}
