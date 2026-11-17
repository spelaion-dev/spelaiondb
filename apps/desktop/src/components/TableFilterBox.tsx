// The WHERE box of a table data tab (SDB-381): it suggests the filters used on the table
// before (newest first) and the table's column names, and saves or drops the filter the tab
// opens with. What is remembered is local data in the store; the core checks every filter
// before it keeps it and again when it reads it back, and the page checks it once more.

import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type { TableFiltersItem } from "../bindings";

/** Suggestions of each kind listed at once. */
const SHOWN = 8;
/** A name the engines read as it is: anything else is quoted by the core. */
const PLAIN_NAME = /^[A-Za-z_][A-Za-z0-9_]*$/;
/** The word being typed just before the caret. */
const WORD = /[\p{L}_][\p{L}\p{N}_$]*$/u;

type Suggestion = { kind: "history"; text: string; isDefault: boolean } | { kind: "column"; text: string };

export function TableFilterBox(props: {
  connectionId: string | null;
  /** The tab's session, which quotes a column name that needs it. */
  session: string;
  schema: string;
  name: string;
  /** The filter of the page on screen. */
  applied: string;
  /** That page loaded without an error: its filter was used. */
  loaded: boolean;
  /** The text in the box. */
  value: string;
  onChange(value: string): void;
  /** Loads the table with this filter at once. */
  onApply(filter: string): void;
  columns: string[];
  disabled: boolean;
}) {
  const { connectionId, schema, name, applied, value } = props;
  const { t } = useTranslation("editor");
  const [memory, setMemory] = useState<TableFiltersItem | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [open, setOpen] = useState(false);
  const [active, setActive] = useState(-1);
  const [caret, setCaret] = useState(0);
  const input = useRef<HTMLInputElement>(null);
  const listId = useId();
  const callbacks = useRef(props);
  callbacks.current = props;

  const failed = (e: unknown) => setError(e instanceof Error ? e.message : String(e));

  // One request to the store at a time, in the order they were made: every answer is the
  // table's filters after that request, so an older answer never lands on top of a newer one
  // (remembering a filter and saving it as the default a moment later).
  const queue = useRef<Promise<unknown>>(Promise.resolve());
  const ask = (request: () => Promise<TableFiltersItem>): Promise<void> => {
    const answered = queue.current.then(request).then(setMemory);
    queue.current = answered.catch(() => undefined);
    return answered;
  };

  useEffect(() => {
    if (!connectionId) return;
    void ask(() => call(commands.tableFilters(connectionId, schema, name))).catch(() => undefined);
    // `ask` only reads refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [connectionId, schema, name]);

  // A page that loaded used its filter: the core checks it and the store remembers it. Only
  // when a load ends, never for a filter that has not run yet.
  const wasLoaded = useRef(props.loaded);
  useEffect(() => {
    const before = wasLoaded.current;
    wasLoaded.current = props.loaded;
    if (before || !props.loaded || !connectionId || !applied.trim()) return;
    void ask(() => call(commands.recordTableFilter(connectionId, schema, name, applied))).catch(() => undefined);
    // `ask` only reads refs.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [props.loaded, connectionId, schema, name, applied]);

  // A page that failed has no columns: the last ones seen still name the table's.
  const [columns, setColumns] = useState(props.columns);
  const columnsKey = props.columns.join("\u0000");
  useEffect(() => {
    if (props.columns.length > 0) setColumns(props.columns);
    // The names, not the array's identity, which is new on every render.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [columnsKey]);

  const isDefault = !!memory?.default && applied.trim() === memory.default;
  const word = WORD.exec(value.slice(0, caret))?.[0] ?? "";
  const suggestions = useMemo<Suggestion[]>(() => {
    const typed = value.trim().toLowerCase();
    const history = (memory?.history ?? [])
      .filter((filter) => filter.toLowerCase() !== typed && filter.toLowerCase().includes(typed))
      .slice(0, SHOWN)
      .map((text): Suggestion => ({ kind: "history", text, isDefault: text === memory?.default }));
    const prefix = word.toLowerCase();
    const named = prefix
      ? columns
          .filter((column) => column.toLowerCase().startsWith(prefix) && column.toLowerCase() !== prefix)
          .slice(0, SHOWN)
          .map((text): Suggestion => ({ kind: "column", text }))
      : [];
    return [...named, ...history];
  }, [memory, value, word, columns]);
  const shown = open && suggestions.length > 0;

  // A column name written into the middle of the text: the caret goes right after it as the
  // box takes the new text, so what is typed next lands there and not at the end.
  const caretAfterChange = useRef<number | null>(null);
  useLayoutEffect(() => {
    const at = caretAfterChange.current;
    if (at === null) return;
    caretAfterChange.current = null;
    input.current?.setSelectionRange(at, at);
  }, [value]);

  async function accept(suggestion: Suggestion) {
    setOpen(false);
    setActive(-1);
    if (suggestion.kind === "history") {
      callbacks.current.onChange(suggestion.text);
      callbacks.current.onApply(suggestion.text);
      return;
    }
    try {
      // A name that needs quoting is written by the core, as it writes the tab's ORDER BY: one
      // column in ascending order is that column's quoted name. Where the core will not order
      // by it (a masked column), the name goes in as it is and the user quotes it.
      const written = PLAIN_NAME.test(suggestion.text)
        ? suggestion.text
        : await call(commands.tableOrderBy(props.session, schema, name, [{ column: suggestion.text, descending: false }])).catch(() => suggestion.text);
      const start = caret - word.length;
      const next = value.slice(0, start) + written + value.slice(caret);
      const at = start + written.length;
      // Placed with the new text (the layout effect below), before the next key lands.
      caretAfterChange.current = at;
      setCaret(at);
      callbacks.current.onChange(next);
      input.current?.focus();
    } catch (e) {
      failed(e);
    }
  }

  async function forget(filter: string) {
    if (!connectionId) return;
    try {
      await ask(() => call(commands.forgetTableFilter(connectionId, schema, name, filter)));
    } catch (e) {
      failed(e);
    }
  }

  /** Saves the filter on screen as the table's default, or drops the default. */
  async function setDefault(filter: string | null) {
    if (!connectionId) return;
    setError(null);
    try {
      await ask(() => call(commands.setTableDefaultFilter(connectionId, schema, name, filter)));
    } catch (e) {
      failed(e);
    }
  }

  const canSave = props.loaded && applied.trim() !== "" && !isDefault;
  const hasDefault = !!memory?.default;
  const defaultLabel = isDefault || (!canSave && hasDefault) ? t("table.dropDefault", { filter: memory?.default ?? "" }) : t("table.saveDefault");
  return (
    <>
      {/* Not a <label>: the list of suggestions sits inside, and the box is named by aria-label. */}
      <div className="table-clause table-filter-box">
        <span className="mono" aria-hidden="true">WHERE</span>
        <input
          ref={input}
          value={value}
          // A plain text box with a list of suggestions: typing or the down arrow opens it.
          aria-label="WHERE"
          aria-controls={shown ? listId : undefined}
          aria-autocomplete="list"
          title={t("table.filterHint")}
          aria-activedescendant={shown && active >= 0 ? `${listId}-${active}` : undefined}
          placeholder={t("table.filterPlaceholder")}
          spellCheck={false}
          autoComplete="off"
          onChange={(e) => {
            props.onChange(e.target.value);
            setCaret(e.target.selectionStart ?? e.target.value.length);
            setOpen(true);
            setActive(-1);
          }}
          onSelect={(e) => setCaret(e.currentTarget.selectionStart ?? 0)}
          onBlur={() => { setOpen(false); setActive(-1); }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              if (suggestions.length === 0) return;
              e.preventDefault();
              setOpen(true);
              const step = e.key === "ArrowDown" ? 1 : -1;
              // From the box (-1) into the list and back out of it.
              setActive((at) => ((at + 1 + step + suggestions.length + 1) % (suggestions.length + 1)) - 1);
            } else if ((e.key === "Enter" || e.key === "Tab") && shown && active >= 0) {
              e.preventDefault();
              void accept(suggestions[active]!);
            } else if (e.key === "Escape" && shown) {
              e.preventDefault();
              e.stopPropagation();
              setOpen(false);
              setActive(-1);
            } else if (e.key === "Enter") {
              // The form applies what is typed: the list has nothing more to offer.
              setOpen(false);
            }
          }}
        />
        {isDefault && <span className="table-default-badge" title={t("table.defaultHint")}>{t("table.defaultBadge")}</span>}
        {shown && (
          <ul id={listId} className="table-filter-suggestions" role="listbox" aria-label={t("table.suggestions")}>
            {suggestions.map((suggestion, index) => (
              <li
                key={`${suggestion.kind}:${suggestion.text}`}
                id={`${listId}-${index}`}
                role="option"
                aria-selected={index === active}
                className={index === active ? "active" : undefined}
                // The box keeps the focus: a click must not blur it before it lands.
                onMouseDown={(e) => { e.preventDefault(); void accept(suggestion); }}
              >
                <code>{suggestion.text}</code>
                {suggestion.kind === "column" && <span className="muted small">{t("table.suggestedColumn")}</span>}
                {suggestion.kind === "history" && suggestion.isDefault && <span className="muted small">{t("table.defaultBadge")}</span>}
                {suggestion.kind === "history" && (
                  <button
                    type="button"
                    className="ghost small"
                    tabIndex={-1}
                    aria-label={t("table.forgetFilter", { filter: suggestion.text })}
                    title={t("table.forgetFilter", { filter: suggestion.text })}
                    onMouseDown={(e) => { e.preventDefault(); e.stopPropagation(); void forget(suggestion.text); }}
                  >
                    ×
                  </button>
                )}
              </li>
            ))}
          </ul>
        )}
      </div>
      <button
        type="button"
        className="ghost table-default-button"
        aria-label={defaultLabel}
        aria-pressed={isDefault}
        title={defaultLabel}
        disabled={props.disabled || !connectionId || (!canSave && !hasDefault)}
        onClick={() => void setDefault(isDefault || !canSave ? null : applied.trim())}
      >
        {isDefault ? "★" : "☆"}
      </button>
      {memory?.refusedDefault && (
        <span className="table-filter-note small" role="alert">
          {t("table.defaultRefused", { reason: memory.refusedDefault.reason })}{" "}
          <button type="button" className="ghost small" onClick={() => void setDefault(null)}>{t("table.dropRefused")}</button>
        </span>
      )}
      {error && <span className="table-filter-note small" role="alert">{t("table.memoryFailed", { reason: error })}</span>}
    </>
  );
}
