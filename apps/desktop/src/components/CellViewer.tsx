// Cell viewer: the full value of the active cell beside the grid. JSON is pretty-printed,
// binary values become a hex dump and long text keeps its line breaks. The row view lists
// every column of the active row instead. Values are asked to the core, so it works for
// rows that are not in the grid's cache.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ColumnItem } from "../api";

type View = { kind: "null" } | { kind: "json"; text: string } | { kind: "hex"; text: string; bytes: number } | { kind: "text"; text: string };

/** `\xdeadbeef` (how the core writes bytes) → offset, hex and ASCII columns, 16 per line. */
export function hexDump(hex: string): { text: string; bytes: number } {
  const digits = hex.startsWith("\\x") ? hex.slice(2) : hex;
  const bytes: number[] = [];
  for (let i = 0; i + 1 < digits.length; i += 2) bytes.push(parseInt(digits.slice(i, i + 2), 16));
  const lines: string[] = [];
  for (let offset = 0; offset < bytes.length; offset += 16) {
    const chunk = bytes.slice(offset, offset + 16);
    const hexPart = chunk.map((b) => b.toString(16).padStart(2, "0")).join(" ").padEnd(16 * 3 - 1, " ");
    const ascii = chunk.map((b) => (b >= 0x20 && b < 0x7f ? String.fromCharCode(b) : ".")).join("");
    lines.push(`${offset.toString(16).padStart(8, "0")}  ${hexPart}  ${ascii}`);
  }
  return { text: lines.join("\n"), bytes: bytes.length };
}

function view(value: string | null, column: ColumnItem): View {
  if (value === null) return { kind: "null" };
  if (column.logical === "bytes") return { kind: "hex", ...hexDump(value) };
  const trimmed = value.trimStart();
  if (column.logical === "json" || trimmed.startsWith("{") || trimmed.startsWith("[")) {
    try {
      return { kind: "json", text: JSON.stringify(JSON.parse(value), null, 2) };
    } catch {
      // Not JSON after all: shown as text.
    }
  }
  return { kind: "text", text: value };
}

export type ViewerMode = "cell" | "row" | "references" | "calc";

const MODE_LABELS: Record<ViewerMode, string> = { cell: "viewer.cellMode", row: "viewer.rowMode", references: "viewer.referencesMode", calc: "viewer.calcMode" };

/** The side panel's views: the cell, its row, in a table data tab the row's references, and the
 *  numbers about the selection. */
export function ViewerModes(props: { modes: ViewerMode[]; mode: ViewerMode; onMode(mode: ViewerMode): void }) {
  const { t } = useTranslation("editor");
  return (
    <div className="viewer-modes" role="group" aria-label={t("viewer.mode")}>
      {props.modes.map((m) => (
        <button key={m} className="ghost small" aria-pressed={props.mode === m} onClick={() => props.onMode(m)}>
          {t(MODE_LABELS[m])}
        </button>
      ))}
    </div>
  );
}

/** Longest value shown per field in the row view; the cell view shows it whole. */
const ROW_VIEW_CHARS = 500;

export function CellViewer(props: {
  resultId: number;
  row: number;
  column: number;
  meta: ColumnItem;
  columns: ColumnItem[];
  mode: ViewerMode;
  /** The views offered (the cell and the row unless said otherwise). */
  modes?: ViewerMode[];
  edits?: Record<string, string | null>;
  editable?: boolean;
  onEdit?(row: number, column: number, value: string | null | undefined): void;
  onMode(mode: ViewerMode): void;
  /** A field of the row view was picked: show that cell. */
  onPickColumn(column: number): void;
  onClose(): void;
}) {
  const { resultId, row, column, meta, mode } = props;
  const { t, i18n } = useTranslation("editor");
  // Each answer remembers its cell, so moving to another cell never shows the previous
  // value with the new column's type while the new one loads.
  const cell = `${resultId}:${row}:${column}`;
  const [loaded, setLoaded] = useState<{ cell: string; text: string | null; error: string | null } | null>(null);
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState<string | null>(null);

  useEffect(() => {
    let active = true;
    setCopied(false);
    setCopyError(null);
    call(commands.cellValue(resultId, row, column))
      .then((text) => active && setLoaded({ cell, text, error: null }))
      .catch((e: unknown) => active && setLoaded({ cell, text: null, error: e instanceof Error ? e.message : String(e) }));
    return () => {
      active = false;
    };
  }, [cell, resultId, row, column]);

  const current = loaded?.cell === cell ? loaded : null;
  const error = current?.error ?? null;
  const editKey = `${row}:${column}`;
  const hasEdit = props.edits != null && editKey in props.edits;
  const value = current && !current.error ? { text: hasEdit ? props.edits![editKey]! : current.text } : null;
  const shown = value ? view(value.text, meta) : null;
  const length =
    shown?.kind === "hex"
      ? t("viewer.bytes", { count: shown.bytes, formattedCount: shown.bytes.toLocaleString(i18n.language) })
      : value?.text != null
        ? t("viewer.characters", { count: [...value.text].length, formattedCount: [...value.text].length.toLocaleString(i18n.language) })
        : "";

  const modes = <ViewerModes modes={props.modes ?? ["cell", "row"]} mode={mode} onMode={props.onMode} />;

  if (mode === "row") {
    return (
      <aside className="cell-viewer" aria-label={t("viewer.title")} onKeyDown={(e) => e.key === "Escape" && props.onClose()}>
        <header>
          <div className="cell-viewer-heading">
            <div className="cell-viewer-title">
              <strong>{t("viewer.rowTitle", { number: (row + 1).toLocaleString(i18n.language) })}</strong>
              <span className="muted small">{t("viewer.columns", { count: props.columns.length })}</span>
            </div>
            <button className="ghost cell-viewer-close" aria-label={t("viewer.close")} onClick={props.onClose}>×</button>
          </div>
          <div className="cell-viewer-controls">{modes}</div>
        </header>
        <RowFields resultId={resultId} row={row} columns={props.columns} active={column} edits={props.edits} onPick={props.onPickColumn} />
      </aside>
    );
  }

  return (
    <aside className="cell-viewer" aria-label={t("viewer.title")} onKeyDown={(e) => e.key === "Escape" && props.onClose()}>
      <header>
        <div className="cell-viewer-heading">
          <div className="cell-viewer-title">
            <strong title={meta.name}>{meta.name}</strong>
            <span className="muted small" title={`${meta.nativeType} · ${t("viewer.row", { number: (row + 1).toLocaleString(i18n.language) })}${length ? ` · ${length}` : ""}`}>
              {meta.nativeType} · {t("viewer.row", { number: (row + 1).toLocaleString(i18n.language) })}
              {length ? ` · ${length}` : ""}
            </span>
          </div>
          <button className="ghost cell-viewer-close" aria-label={t("viewer.close")} onClick={props.onClose}>×</button>
        </div>
        <div className="cell-viewer-controls">
          {modes}
          <button
            className="ghost small"
            disabled={!value || value.text === null}
            onClick={() => {
              if (value?.text != null) void navigator.clipboard.writeText(value.text).then(() => { setCopied(true); setCopyError(null); }, (e: unknown) => { setCopied(false); setCopyError(e instanceof Error ? e.message : String(e)); });
            }}
          >
            {copied ? t("viewer.copied") : t("viewer.copy")}
          </button>
        </div>
      </header>
      {error && <p className="error small" role="alert">{error}</p>}
      {copyError && <p className="error small" role="alert">{t("viewer.copyError", { detail: copyError })}</p>}
      {!value && !error && <p className="muted small">{t("viewer.loading")}</p>}
      {value && props.editable && props.onEdit && (
        <>
          <div className="viewer-value-heading">{t("viewer.editValue")}</div>
          <textarea
            className="cell-viewer-value cell-viewer-editor"
            aria-label={t("edit.cellLabel", { column: meta.name })}
            spellCheck={false}
            value={value.text ?? ""}
            placeholder={value.text === null ? "NULL" : undefined}
            onChange={(e) => { setCopied(false); setCopyError(null); props.onEdit?.(row, column, e.target.value === current?.text ? undefined : e.target.value); }}
          />
          <footer className="viewer-edit-actions" role="toolbar" aria-label={t("viewer.editActions")}>
            <span className="muted small">{t(hasEdit ? "viewer.pendingEdit" : "viewer.editHint")}</span>
            <button className="ghost small" title={t("edit.setNullHint")} disabled={value.text === null} onClick={() => props.onEdit?.(row, column, current?.text === null ? undefined : null)}>{t("edit.setNull")}</button>
          </footer>
        </>
      )}
      {!props.editable && shown?.kind === "null" && <p className="muted cell-viewer-null">NULL</p>}
      {!props.editable && shown && shown.kind !== "null" && (
        <pre className={`cell-viewer-value cell-viewer-${shown.kind}`} tabIndex={0}>
          {shown.text}
        </pre>
      )}
    </aside>
  );
}

function RowFields(props: { resultId: number; row: number; columns: ColumnItem[]; active: number; edits?: Record<string, string | null>; onPick(column: number): void }) {
  const { resultId, row } = props;
  const { t } = useTranslation("editor");
  const key = `${resultId}:${row}`;
  const [loaded, setLoaded] = useState<{ key: string; values: (string | null)[] } | { key: string; error: string } | null>(null);

  useEffect(() => {
    let active = true;
    call(commands.rowValues(resultId, row))
      .then((values) => active && setLoaded({ key, values }))
      .catch((e: unknown) => active && setLoaded({ key, error: e instanceof Error ? e.message : String(e) }));
    return () => {
      active = false;
    };
  }, [key, resultId, row]);

  const current = loaded?.key === key ? loaded : null;
  if (!current) return <p className="muted small">{t("viewer.loading")}</p>;
  if ("error" in current) return <p className="error small" role="alert">{current.error}</p>;
  return (
    <dl className="row-view">
      {props.columns.map((c, i) => {
        const key = `${row}:${i}`;
        const value = props.edits && key in props.edits ? props.edits[key] ?? null : current.values[i] ?? null;
        return (
          <div key={i} className={`row-field ${i === props.active ? "active" : ""}`} onClick={() => props.onPick(i)} title={t("viewer.pick")}>
            <dt>
              <button type="button" className="row-field-button" aria-label={t("viewer.pickColumn", { name: c.name })}>{c.name}</button> <span className="muted small">{c.nativeType}</span>
            </dt>
            <dd className={value === null ? "muted" : ""}>
              {value === null ? "NULL" : value.length > ROW_VIEW_CHARS ? `${value.slice(0, ROW_VIEW_CHARS)}…` : value}
            </dd>
          </div>
        );
      })}
    </dl>
  );
}
