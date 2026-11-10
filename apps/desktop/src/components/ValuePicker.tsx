// The cell panel's value pickers in an editable result: a date and time for date, time and
// timestamp columns, the allowed values of an enum, a set or a CHECK list, and a search of the
// table a foreign key references. The core says which picker a column takes (from the catalog),
// writes the text of what is picked (the same text the grid shows once it is saved) and pages
// the referenced table; picking only sets the cell's pending edit, saved like any other edit.

import { useEffect, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ColumnItem } from "../api";
import type {
  ChoicePickerItem,
  KeyPickerItem,
  ReferencedRowsItem,
  TemporalPickerItem,
  ValuePickerItem_Serialize as ValuePickerItem,
} from "../bindings";

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** One edit a pick makes: the result column and its new text. */
export interface PickedEdit {
  column: number;
  value: string;
}

/** The table a value picker reads its column from. */
export interface PickerSource {
  session: string;
  schema: string;
  table: string;
}

/** Asks the core which picker `column` takes; `null` when none (or until known). */
export function usePicker(source: PickerSource | undefined, column: string): ValuePickerItem | null {
  const [loaded, setLoaded] = useState<{ key: string; picker: ValuePickerItem | null } | null>(null);
  const key = source ? `${source.session}\u0000${source.schema}\u0000${source.table}\u0000${column}` : "";
  useEffect(() => {
    if (!source) return;
    let active = true;
    call(commands.valuePicker(source.session, source.schema, source.table, column))
      .then((picker) => active && setLoaded({ key, picker: picker ?? null }))
      // A column whose catalog cannot be read just takes no picker: typing still works.
      .catch(() => active && setLoaded({ key, picker: null }));
    return () => {
      active = false;
    };
  }, [key, source?.session, source?.schema, source?.table, column]);
  return loaded?.key === key ? loaded.picker : null;
}

export function ValuePicker(props: {
  picker: ValuePickerItem;
  source: PickerSource;
  /** The column the panel shows (a name of the result). */
  column: string;
  /** The cell's text now (the pending edit, if any); `null` for NULL. */
  text: string | null;
  columns: ColumnItem[];
  /** The row's current text for each result column (for a key of several columns). */
  rowText(column: number): string | null;
  onPick(edits: PickedEdit[]): void;
}) {
  const { picker } = props;
  if (picker.kind === "temporal" && picker.temporal) {
    return <TemporalPicker picker={picker.temporal} column={props.column} text={props.text} columns={props.columns} onPick={props.onPick} />;
  }
  if (picker.kind === "choice" && picker.choice) {
    return <ChoicePicker picker={picker.choice} column={props.column} text={props.text} columns={props.columns} onPick={props.onPick} />;
  }
  if (picker.kind === "foreignKey" && picker.key) {
    return <KeyPicker picker={picker.key} source={props.source} column={props.column} columns={props.columns} rowText={props.rowText} onPick={props.onPick} />;
  }
  return null;
}

// ---------------------------------------------------------------- date and time

interface TimeParts {
  date: string; // yyyy-mm-dd, as the date input writes it; "" when not set
  time: string; // hh:mm or hh:mm:ss, as the time input writes it; "" when not set
  fraction: string;
  offset: string; // +hh:mm
}

/** The parts of a date, time or timestamp as the grid writes it (`2026-01-31 10:20:30.5+00`). */
export function parseTimeText(text: string | null): TimeParts {
  const parts: TimeParts = { date: "", time: "", fraction: "", offset: "+00:00" };
  if (!text) return parts;
  const date = /^(\d{4}-\d{2}-\d{2})/.exec(text);
  if (date) parts.date = date[1]!;
  const time = /(?:^|[ T])(\d{2}:\d{2}:\d{2})(?:\.(\d+))?/.exec(text);
  if (time) {
    parts.time = time[1]!;
    parts.fraction = time[2] ?? "";
  }
  const offset = /\d([+-])(\d{2})(?::?(\d{2}))?$/.exec(text);
  if (offset && parts.time) parts.offset = `${offset[1]}${offset[2]}:${offset[3] ?? "00"}`;
  return parts;
}

/** `+05:30` → 330; `null` when it is not an offset. */
export function offsetMinutes(text: string): number | null {
  const m = /^([+-])(\d{1,2})(?::?(\d{2}))?$/.exec(text.trim());
  if (!m) return null;
  const minutes = Number(m[2]) * 60 + Number(m[3] ?? 0);
  return m[1] === "-" ? -minutes : minutes;
}

function pad(n: number, width = 2): string {
  return String(Math.abs(n)).padStart(width, "0");
}

/** The parts of this moment on this computer. */
function nowParts(picker: TemporalPickerItem): TimeParts {
  const now = new Date();
  const offset = -now.getTimezoneOffset();
  const fraction = pad(now.getMilliseconds(), 3).slice(0, Math.min(3, picker.digits));
  return {
    date: `${pad(now.getFullYear(), 4)}-${pad(now.getMonth() + 1)}-${pad(now.getDate())}`,
    time: picker.step === "minutes"
      ? `${pad(now.getHours())}:${pad(now.getMinutes())}:00`
      : `${pad(now.getHours())}:${pad(now.getMinutes())}:${pad(now.getSeconds())}`,
    fraction: picker.step === "minutes" ? "" : fraction.replace(/0+$/, ""),
    offset: `${offset < 0 ? "-" : "+"}${pad(Math.floor(Math.abs(offset) / 60))}:${pad(Math.abs(offset) % 60)}`,
  };
}

function TemporalPicker(props: { picker: TemporalPickerItem; column: string; text: string | null; columns: ColumnItem[]; onPick(edits: PickedEdit[]): void }) {
  const { picker } = props;
  const { t } = useTranslation("editor");
  const [parts, setParts] = useState<TimeParts>(() => parseTimeText(props.text));
  const [error, setError] = useState<string | null>(null);
  // The text this picker wrote last: a change of the cell's text from anywhere else (typing,
  // Set NULL, undo) is read back into the fields.
  const written = useRef<string | null | undefined>(undefined);
  useEffect(() => {
    if (props.text !== written.current) {
      setParts(parseTimeText(props.text));
      setError(null);
    }
  }, [props.text]);

  const hasDate = picker.kind !== "time";
  const hasTime = picker.kind !== "date";
  const hasFraction = hasTime && picker.digits > 0 && picker.step !== "minutes";
  const hasOffset = picker.kind === "timestampTz";
  const columnIndex = props.columns.findIndex((c) => c.name === props.column);

  function apply(next: TimeParts) {
    setParts(next);
    if ((hasDate && !next.date) || (hasTime && !next.time)) return;
    const [year, month, day] = hasDate ? next.date.split("-").map(Number) : [1970, 1, 1];
    const [hour, minute, second] = hasTime ? next.time.split(":").map(Number) : [0, 0, 0];
    const offset = hasOffset ? offsetMinutes(next.offset) : 0;
    if (offset === null) {
      setError(t("picker.badOffset"));
      return;
    }
    call(commands.pickTime(picker, {
      year: year ?? 0,
      month: month ?? 0,
      day: day ?? 0,
      hour: hour ?? 0,
      minute: minute ?? 0,
      second: second ?? 0,
      fraction: hasFraction ? next.fraction : "",
      offsetMinutes: offset,
    }))
      .then((text) => {
        setError(null);
        written.current = text;
        if (columnIndex >= 0) props.onPick([{ column: columnIndex, value: text }]);
      })
      .catch((e: unknown) => setError(errorText(e)));
  }

  const hint = picker.step === "sqlServerDatetime"
    ? t("picker.datetimeHint")
    : picker.step === "minutes"
      ? t("picker.minutesHint")
      : hasFraction
        ? t("picker.fractionHint", { count: picker.digits })
        : null;

  return (
    <section className="value-picker" aria-label={t("picker.dateTitle")}>
      <div className="viewer-value-heading">{t("picker.dateTitle")}</div>
      <div className="value-picker-fields">
        {hasDate && (
          <label className="value-picker-field">
            <span>{t("picker.date")}</span>
            <input type="date" min="0001-01-01" max="9999-12-31" value={parts.date} onChange={(e) => apply({ ...parts, date: e.target.value })} />
          </label>
        )}
        {hasTime && (
          <label className="value-picker-field">
            <span>{t("picker.time")}</span>
            <input
              type="time"
              step={picker.step === "minutes" ? 60 : 1}
              value={picker.step === "minutes" ? parts.time.slice(0, 5) : parts.time}
              onChange={(e) => {
                const value = e.target.value;
                // Without seconds the input writes hh:mm.
                apply({ ...parts, time: value && value.length === 5 ? `${value}:00` : value });
              }}
            />
          </label>
        )}
        {hasFraction && (
          <label className="value-picker-field value-picker-fraction">
            <span>{t("picker.fraction")}</span>
            <input
              type="text"
              inputMode="numeric"
              spellCheck={false}
              maxLength={picker.digits}
              placeholder={"0".repeat(picker.digits)}
              value={parts.fraction}
              onChange={(e) => apply({ ...parts, fraction: e.target.value })}
            />
          </label>
        )}
        {hasOffset && (
          <label className="value-picker-field value-picker-offset">
            <span>{t("picker.offset")}</span>
            <input type="text" spellCheck={false} maxLength={6} placeholder="+00:00" value={parts.offset} onChange={(e) => apply({ ...parts, offset: e.target.value })} />
          </label>
        )}
        <button type="button" className="ghost small" onClick={() => apply(nowParts(picker))}>{t(picker.kind === "date" ? "picker.today" : "picker.now")}</button>
      </div>
      {hasOffset && <p className="muted small value-picker-note">{t("picker.utcHint")}</p>}
      {hint && <p className="muted small value-picker-note">{hint}</p>}
      {error && <p className="error small value-picker-note" role="alert">{error}</p>}
    </section>
  );
}

// ---------------------------------------------------------------- allowed values

/** Past this many values a list gets a filter box. */
const FILTER_FROM = 12;

function ChoicePicker(props: { picker: ChoicePickerItem; column: string; text: string | null; columns: ColumnItem[]; onPick(edits: PickedEdit[]): void }) {
  const { picker } = props;
  const { t } = useTranslation("editor");
  const [filter, setFilter] = useState("");
  const [error, setError] = useState<string | null>(null);
  const multiple = picker.source === "set";
  const columnIndex = props.columns.findIndex((c) => c.name === props.column);
  const stored = useMemo(
    () => (props.text === null ? [] : multiple ? (props.text === "" ? [] : props.text.split(",")) : [props.text]),
    [props.text, multiple],
  );
  // What was just picked, shown at once while the core writes its text; the cell's text wins
  // as soon as it changes, and a refused pick goes back to it.
  const [picked, setPicked] = useState<{ base: string | null; values: string[] } | null>(null);
  const chosen = picked && picked.base === props.text ? picked.values : stored;
  const shown = filter.trim() ? picker.values.filter((v) => v.toLowerCase().includes(filter.trim().toLowerCase())) : picker.values;

  function pick(values: string[]) {
    setPicked({ base: props.text, values });
    call(commands.pickChoices(picker, values))
      .then((text) => {
        setError(null);
        if (columnIndex >= 0) props.onPick([{ column: columnIndex, value: text }]);
      })
      .catch((e: unknown) => {
        setPicked(null);
        setError(errorText(e));
      });
  }

  const sourceNote = picker.source === "enum" ? t("picker.fromEnum") : picker.source === "set" ? t("picker.fromSet") : t("picker.fromCheck");
  return (
    <section className="value-picker value-picker-grow" aria-label={t("picker.choiceTitle")}>
      <div className="viewer-value-heading">{t(multiple ? "picker.setTitle" : "picker.choiceTitle")}</div>
      <p className="muted small value-picker-note">{sourceNote}</p>
      {picker.values.length > FILTER_FROM && (
        <input
          type="search"
          className="value-picker-search"
          aria-label={t("picker.filterValues")}
          placeholder={t("picker.filterValues")}
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
        />
      )}
      <div className="value-picker-list" role="group" aria-label={t("picker.values", { column: props.column })}>
        {shown.map((value) => {
          const checked = chosen.includes(value);
          return (
            <label key={value} className={`value-picker-option ${checked ? "checked" : ""}`}>
              <input
                type={multiple ? "checkbox" : "radio"}
                name={`pick-${props.column}`}
                checked={checked}
                onChange={() => pick(multiple ? (checked ? chosen.filter((c) => c !== value) : [...chosen.filter((c) => picker.values.includes(c)), value]) : [value])}
              />
              <span className="value-picker-value">{value === "" ? t("picker.emptyText") : value}</span>
            </label>
          );
        })}
        {shown.length === 0 && <p className="muted small value-picker-note">{t("picker.noValues")}</p>}
      </div>
      {error && <p className="error small value-picker-note" role="alert">{error}</p>}
    </section>
  );
}

// ---------------------------------------------------------------- foreign key

/** Waits this long after the last key before searching. */
const SEARCH_DELAY_MS = 300;

function KeyPicker(props: {
  picker: KeyPickerItem;
  source: PickerSource;
  column: string;
  columns: ColumnItem[];
  rowText(column: number): string | null;
  onPick(edits: PickedEdit[]): void;
}) {
  const { picker, source } = props;
  const { t, i18n } = useTranslation("editor");
  const [search, setSearch] = useState("");
  const [query, setQuery] = useState({ search: "", offset: 0 });
  const [page, setPage] = useState<{ key: string; rows: ReferencedRowsItem } | { key: string; error: string } | null>(null);
  const requestKey = `${source.session}\u0000${source.schema}\u0000${source.table}\u0000${props.column}\u0000${query.search}\u0000${query.offset}`;

  useEffect(() => {
    const timer = window.setTimeout(() => setQuery((q) => (q.search === search ? q : { search, offset: 0 })), SEARCH_DELAY_MS);
    return () => window.clearTimeout(timer);
  }, [search]);

  useEffect(() => {
    let active = true;
    call(commands.searchReferenced(source.session, source.schema, source.table, props.column, query.search, query.offset))
      .then((rows) => active && setPage({ key: requestKey, rows }))
      .catch((e: unknown) => active && setPage({ key: requestKey, error: errorText(e) }));
    return () => {
      active = false;
    };
  }, [requestKey]);

  // Where each column of the key is in the result; a key column the result lacks cannot be set.
  const targets = picker.columns.map((name) => props.columns.findIndex((c) => c.name === name));
  const missing = picker.columns.filter((_, i) => targets[i]! < 0);
  const current = targets.map((i) => (i >= 0 ? props.rowText(i) : null));
  const loading = page?.key !== requestKey;
  const rows = page && "rows" in page ? page.rows : null;
  const error = page && "error" in page && !loading ? page.error : null;
  const numbers = new Intl.NumberFormat(i18n.language);

  function pick(row: (string | null)[]) {
    if (missing.length > 0) return;
    const edits: PickedEdit[] = [];
    targets.forEach((column, i) => {
      const value = row[i];
      if (value != null) edits.push({ column, value });
    });
    props.onPick(edits);
  }

  return (
    <section className="value-picker value-picker-grow" aria-label={t("picker.keyTitle", { table: picker.table })}>
      <div className="viewer-value-heading">{t("picker.keyTitle", { table: picker.table })}</div>
      <p className="muted small value-picker-note">
        {t("picker.keyNote", { columns: picker.columns.join(", "), table: `${picker.table} (${picker.referencedColumns.join(", ")})` })}
      </p>
      {missing.length > 0 && <p className="muted small value-picker-note" role="status">{t("picker.keyMissing", { columns: missing.join(", ") })}</p>}
      <input
        type="search"
        className="value-picker-search"
        aria-label={t("picker.searchTable", { table: picker.table })}
        placeholder={t("picker.searchTable", { table: picker.table })}
        value={search}
        onChange={(e) => setSearch(e.target.value)}
      />
      {error && <p className="error small value-picker-note" role="alert">{t("picker.searchError", { detail: error })}</p>}
      {rows && (
        <div className="value-picker-rows" role="list" aria-busy={loading} aria-label={t("picker.rowsOf", { table: picker.table })}>
          <div className="value-picker-row value-picker-row-head" aria-hidden="true">
            {rows.columns.map((c, i) => <span key={i} className={i < rows.keyColumns ? "key" : ""}>{c}</span>)}
          </div>
          {rows.rows.map((row, r) => {
            const selected = row.slice(0, rows.keyColumns).every((v, i) => v !== null && v === current[i]);
            return (
              <div role="listitem" key={`${rows.offset}-${r}`}>
                <button
                  type="button"
                  className={`value-picker-row ${selected ? "selected" : ""}`}
                  aria-pressed={selected}
                  disabled={missing.length > 0}
                  title={t("picker.useRow")}
                  onClick={() => pick(row)}
                >
                  {row.map((v, i) => (
                    <span key={i} className={`${i < rows.keyColumns ? "key" : ""} ${v === null ? "muted" : ""}`}>{v ?? "NULL"}</span>
                  ))}
                </button>
              </div>
            );
          })}
          {rows.rows.length === 0 && !loading && (
            <p className="muted small value-picker-note">{query.search ? t("picker.noMatches") : t("picker.noRows", { table: picker.table })}</p>
          )}
        </div>
      )}
      {!rows && loading && <p className="muted small value-picker-note">{t("picker.searching")}</p>}
      {rows && (rows.offset > 0 || rows.hasMore) && (
        <footer className="value-picker-pages">
          <span className="muted small">
            {t("picker.rowRange", { from: numbers.format(rows.offset + 1), to: numbers.format(rows.offset + rows.rows.length) })}
          </span>
          <button type="button" className="ghost small" disabled={rows.offset === 0 || loading} onClick={() => setQuery((q) => ({ ...q, offset: Math.max(0, q.offset - rows.pageRows) }))}>
            {t("picker.previous")}
          </button>
          <button type="button" className="ghost small" disabled={!rows.hasMore || loading} onClick={() => setQuery((q) => ({ ...q, offset: q.offset + rows.pageRows }))}>
            {t("picker.next")}
          </button>
        </footer>
      )}
    </section>
  );
}
