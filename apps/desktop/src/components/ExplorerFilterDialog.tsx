// A connection's explorer filter (ADR-025): include and exclude patterns for its schemas,
// objects and columns, kept in the personal settings. The core matches them.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ConnectionItem, type ExplorerFilter, type NameFilter } from "../api";

export type FilterKind = "schemas" | "objects" | "columns";
type Kind = FilterKind;
const KINDS: Kind[] = ["schemas", "objects", "columns"];

/** Patterns as typed: separated by commas, blanks dropped. */
function parsePatterns(text: string): string[] {
  return text
    .split(",")
    .map((p) => p.trim())
    .filter((p) => p !== "");
}

/** Whether a filter hides nothing: all of it, or only one kind. */
export function isFilterEmpty(filter: ExplorerFilter | null, kind?: FilterKind): boolean {
  return !filter || (kind ? [kind] : KINDS).every((k) => filter[k].include.length === 0 && filter[k].exclude.length === 0);
}

export function ExplorerFilterDialog({
  connection,
  filter,
  kind,
  onSaved,
  onClose,
}: {
  connection: ConnectionItem;
  filter: ExplorerFilter;
  /** Only this kind, as when it is opened from a folder; otherwise all three. */
  kind?: FilterKind;
  onSaved(filter: ExplorerFilter): void;
  onClose(): void;
}) {
  const { t } = useTranslation("connections");
  const kinds = kind ? [kind] : KINDS;
  const title = kind
    ? t("filterTitleKind", { kind: t(`filterKinds.${kind}`), name: connection.name })
    : t("filterTitle", { name: connection.name });
  const [text, setText] = useState(() =>
    Object.fromEntries(
      KINDS.flatMap((k) => [
        [`${k}.include`, filter[k].include.join(", ")],
        [`${k}.exclude`, filter[k].exclude.join(", ")],
      ]),
    ) as Record<string, string>,
  );
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const build = (from: Record<string, string>): ExplorerFilter => {
    const one = (k: Kind): NameFilter => ({
      include: parsePatterns(from[`${k}.include`] ?? ""),
      exclude: parsePatterns(from[`${k}.exclude`] ?? ""),
    });
    return { schemas: one("schemas"), objects: one("objects"), columns: one("columns") };
  };

  async function save(next: ExplorerFilter) {
    setSaving(true);
    setError(null);
    try {
      await call(commands.setExplorerFilter(connection.id, next));
      onSaved(next);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <form
        className="modal explorer-filter-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onSubmit={(e) => {
          e.preventDefault();
          void save(build(text));
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") onClose();
        }}
      >
        <h2>{title}</h2>
        <p className="muted small">{t("filterHint")}</p>
        {kinds.map((k) => (
          <fieldset key={k}>
            <legend>{t(`filterKinds.${k}`)}</legend>
            {(["include", "exclude"] as const).map((side) => (
              <label key={side} className="field">
                <span>{t(`filter.${side}`)}</span>
                <input
                  className="mono"
                  name={`${k}.${side}`}
                  value={text[`${k}.${side}`]}
                  onChange={(e) => setText({ ...text, [`${k}.${side}`]: e.target.value })}
                  placeholder={side === "include" ? t("filterIncludePlaceholder") : t("filterExcludePlaceholder")}
                />
              </label>
            ))}
          </fieldset>
        ))}
        <p className="muted small">{t("filterPersonal")}</p>
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button
            type="button"
            onClick={() => {
              const cleared = { ...filter };
              for (const k of kinds) cleared[k] = { include: [], exclude: [] };
              void save(cleared);
            }}
            disabled={saving || isFilterEmpty(filter, kind)}
          >
            {t("filterClear")}
          </button>
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={saving}>
            {t("cancel")}
          </button>
          <button type="submit" className="primary" disabled={saving}>
            {t("filterApply")}
          </button>
        </div>
      </form>
    </div>
  );
}
