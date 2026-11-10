// Cell viewer: the full value of the active cell beside the grid. JSON and XML are
// pretty-printed, images the core recognised by their bytes (PNG, JPEG, GIF, WebP) are
// previewed, other binary values become a hex dump and long text keeps its line breaks. The
// row view lists every column of the active row instead. Values are asked to the core when
// the viewer opens, so it works for rows that are not in the grid's cache; a value past the
// core's limit waits for "Load anyway". A value can be saved to a file, and in an editable
// table a file can be loaded into it (one UPDATE by primary key, written by the core).

import type { TFunction } from "i18next";
import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ColumnItem } from "../api";
import type { CellViewItem_Serialize as CellViewItem } from "../bindings";
import { chooseOpenFile, chooseSavePath } from "../dialogs";
import { usePicker, ValuePicker, type PickedEdit, type PickerSource } from "./ValuePicker";

type View =
  | { kind: "null" }
  | { kind: "json"; text: string }
  | { kind: "xml"; text: string }
  | { kind: "hex"; text: string; bytes: number }
  | { kind: "text"; text: string }
  | { kind: "image"; url: string; format: string };

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

/** Names of the image formats the core recognises (brand names: not translated). */
const IMAGE_FORMATS: Record<string, string> = { png: "PNG", jpeg: "JPEG", gif: "GIF", webp: "WebP" };

function formatSize(n: number, language: string): string {
  if (n < 1024) return `${new Intl.NumberFormat(language).format(n)} B`;
  if (n < 1024 * 1024) return `${new Intl.NumberFormat(language).format(Math.round(n / 1024))} KB`;
  return `${new Intl.NumberFormat(language, { maximumFractionDigits: 1 }).format(n / (1024 * 1024))} MB`;
}

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/** Asks where to save a cell's value (named after its column, with the extension the core
 *  gives) and has the core write it: the note to show, or `null` if the user cancelled. */
export async function saveValueToFile(
  resultId: number,
  row: number,
  column: number,
  columnName: string,
  extension: string,
  t: TFunction<"editor">,
  language: string,
): Promise<{ text: string; error: boolean } | null> {
  const path = await chooseSavePath({
    title: t("viewer.saveTitle"),
    defaultPath: `${columnName}.${extension}`,
    filters: [{ name: extension.toUpperCase(), extensions: [extension] }, { name: t("viewer.allFiles"), extensions: ["*"] }],
  });
  if (!path) return null;
  try {
    const written = await call(commands.saveCellValue(resultId, row, column, path));
    return { text: t("viewer.saved", { size: formatSize(written ?? 0, language), path }), error: false };
  } catch (e) {
    return { text: t("viewer.saveError", { detail: errorText(e) }), error: true };
  }
}

/** Asks for the file to load into a cell; `null` if the user cancelled. */
export function chooseValueFile(t: TFunction<"editor">): Promise<string | null> {
  return chooseOpenFile(t("viewer.loadTitle"), [{ name: t("viewer.allFiles"), extensions: ["*"] }]);
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
  /** Keys of `edits` set to the column's default. */
  defaults?: Set<string>;
  editable?: boolean;
  /** The tab has changes not yet saved: a file is loaded only when there are none. */
  pendingChanges?: boolean;
  onEdit?(row: number, column: number, value: string | null | undefined): void;
  /** A keystroke in the value's text box: typing on in one cell is one undo step. */
  onType?(row: number, column: number, value: string | null | undefined): void;
  /** Loads the file at `path` into this cell now (the core writes one UPDATE by key). */
  onLoadFile?(row: number, column: number, path: string): Promise<void>;
  /** The table an editable result's rows are from: its columns may take a value picker. */
  pickerSource?: PickerSource;
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
  const [anyway, setAnyway] = useState<string | null>(null);
  const loadAnyway = anyway === cell;
  const [loaded, setLoaded] = useState<{ cell: string; item: CellViewItem | null; error: string | null } | null>(null);
  const [copied, setCopied] = useState(false);
  const [copyError, setCopyError] = useState<string | null>(null);
  const [fileNote, setFileNote] = useState<{ cell: string; text: string; error: boolean } | null>(null);
  const [loadingFile, setLoadingFile] = useState(false);
  const [binaryView, setBinaryView] = useState<"image" | "hex">("image");
  const [image, setImage] = useState<{ cell: string; width: number; height: number } | { cell: string; failed: true } | null>(null);

  useEffect(() => {
    let active = true;
    setCopied(false);
    setCopyError(null);
    call(commands.viewCell(resultId, row, column, loadAnyway))
      .then((item) => active && setLoaded({ cell, item, error: null }))
      .catch((e: unknown) => active && setLoaded({ cell, item: null, error: errorText(e) }));
    return () => {
      active = false;
    };
  }, [cell, resultId, row, column, loadAnyway]);

  const current = loaded?.cell === cell ? loaded : null;
  const item = current?.item ?? null;
  const error = current?.error ?? null;
  const editKey = `${row}:${column}`;
  const hasEdit = props.edits != null && editKey in props.edits;
  const editText = hasEdit ? props.edits![editKey]! : null;
  const withheld = !hasEdit && (item?.kind === "withheld" || item?.kind === "tooBig");
  /** The value as text (`\x…` for bytes), when it is known: what Copy copies and the editor edits. */
  const value = item && !withheld ? { text: hasEdit ? editText : item.text } : null;
  const imageInfo = image?.cell === cell ? image : null;
  const shown: View | null = !item || withheld
    ? null
    : hasEdit
      ? view(editText, meta)
      : item.kind === "null"
        ? { kind: "null" }
        : item.kind === "bytes" && item.imageUrl && item.image && binaryView === "image" && !(imageInfo && "failed" in imageInfo)
          ? { kind: "image", url: item.imageUrl, format: IMAGE_FORMATS[item.image] ?? item.image }
          : item.kind === "text" && item.xml
            ? { kind: "xml", text: item.xml }
            : view(item.text, meta);
  const size = item?.size ?? 0;
  const length = hasEdit
    ? ""
    : item?.binary && item.kind !== "null"
      ? t("viewer.bytes", { count: size, formattedCount: size.toLocaleString(i18n.language) })
      : withheld
        ? formatSize(size, i18n.language)
        : value?.text != null
          ? t("viewer.characters", { count: [...value.text].length, formattedCount: [...value.text].length.toLocaleString(i18n.language) })
          : "";
  const note = fileNote?.cell === cell ? fileNote : null;

  async function saveToFile() {
    if (!item) return;
    const saved = await saveValueToFile(resultId, row, column, meta.name, item.extension, t, i18n.language);
    if (saved) setFileNote({ cell, ...saved });
  }

  async function loadFromFile() {
    if (!props.onLoadFile) return;
    const path = await chooseValueFile(t);
    if (!path) return;
    setLoadingFile(true);
    try {
      await props.onLoadFile(row, column, path);
    } finally {
      setLoadingFile(false);
    }
  }

  const pickerEnabled = mode === "cell" && !!props.editable && !!props.onEdit && meta.logical !== "bytes";
  const picker = usePicker(pickerEnabled ? props.pickerSource : undefined, meta.name);
  // A foreign key's other columns: what the row holds now, for a key of several columns.
  const rowKey = `${resultId}:${row}`;
  const [rowValues, setRowValues] = useState<{ key: string; values: (string | null)[] } | null>(null);
  const needsRow = picker?.kind === "foreignKey";
  useEffect(() => {
    if (!needsRow) return;
    let active = true;
    call(commands.rowValues(resultId, row))
      .then((values) => active && setRowValues({ key: rowKey, values }))
      .catch(() => active && setRowValues({ key: rowKey, values: [] }));
    return () => {
      active = false;
    };
  }, [needsRow, rowKey, resultId, row]);
  const storedRow = rowValues?.key === rowKey ? rowValues.values : [];
  const rowText = (col: number): string | null => {
    const key = `${row}:${col}`;
    if (props.edits && key in props.edits) return props.edits[key] ?? null;
    if (col === column) return item && !withheld ? item.text : null;
    return storedRow[col] ?? null;
  };
  function applyPicked(edits: PickedEdit[]) {
    setCopied(false);
    setCopyError(null);
    for (const edit of edits) {
      const original = edit.column === column ? item?.text : storedRow[edit.column];
      props.onEdit?.(row, edit.column, edit.value === original ? undefined : edit.value);
    }
  }

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

  const editable = !!(props.editable && props.onEdit);
  const binary = meta.logical === "bytes";
  const canLoad = editable && !!props.onLoadFile;
  const loadBlocked = props.pendingChanges || loadingFile;
  const preview = shown && shown.kind !== "null" && (
    shown.kind === "image" ? (
      <div className="cell-viewer-image" tabIndex={0}>
        <img
          src={shown.url}
          alt={t("viewer.imageAlt", { format: shown.format, column: meta.name })}
          onLoad={(e) => setImage({ cell, width: e.currentTarget.naturalWidth, height: e.currentTarget.naturalHeight })}
          onError={() => setImage({ cell, failed: true })}
        />
      </div>
    ) : (
      <pre className={`cell-viewer-value cell-viewer-${shown.kind}`} tabIndex={0}>
        {shown.text}
      </pre>
    )
  );

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
          {!hasEdit && item?.kind === "bytes" && item.imageUrl && (
            <div className="viewer-modes" role="group" aria-label={t("viewer.showAs")}>
              <button className="ghost small" aria-pressed={binaryView === "image"} onClick={() => setBinaryView("image")}>{t("viewer.asImage")}</button>
              <button className="ghost small" aria-pressed={binaryView === "hex"} onClick={() => setBinaryView("hex")}>{t("viewer.asHex")}</button>
            </div>
          )}
          <div className="cell-viewer-actions">
            <button
              className="ghost small"
              disabled={!value || value.text === null}
              onClick={() => {
                if (value?.text != null) void navigator.clipboard.writeText(value.text).then(() => { setCopied(true); setCopyError(null); }, (e: unknown) => { setCopied(false); setCopyError(errorText(e)); });
              }}
            >
              {copied ? t("viewer.copied") : t("viewer.copy")}
            </button>
            <button
              className="ghost small"
              title={hasEdit ? t("viewer.saveEditedHint") : undefined}
              disabled={!item || item.kind === "null" || hasEdit}
              onClick={() => void saveToFile()}
            >
              {t("viewer.save")}
            </button>
          </div>
        </div>
      </header>
      {error && <p className="error small" role="alert">{error}</p>}
      {copyError && <p className="error small" role="alert">{t("viewer.copyError", { detail: copyError })}</p>}
      {note && <p className={note.error ? "error small" : "muted small"} role={note.error ? "alert" : "status"}>{note.text}</p>}
      {!item && !error && <p className="muted small">{t("viewer.loading")}</p>}
      {withheld && item && (
        <div className="cell-viewer-notice" role="status">
          <p>
            {item.kind === "tooBig"
              ? t("viewer.tooBig", { size: formatSize(size, i18n.language) })
              : t("viewer.withheld", { size: formatSize(size, i18n.language), limit: formatSize(item.limit ?? 0, i18n.language) })}
          </p>
          {item.image && <p className="muted small">{t("viewer.withheldImage", { format: IMAGE_FORMATS[item.image] ?? item.image })}</p>}
          {item.kind === "withheld" && <button className="small" onClick={() => setAnyway(cell)}>{t("viewer.loadAnyway")}</button>}
        </div>
      )}
      {shown?.kind === "image" && imageInfo && "width" in imageInfo && (
        <p className="muted small">{t("viewer.imageSize", { format: shown.format, width: imageInfo.width.toLocaleString(i18n.language), height: imageInfo.height.toLocaleString(i18n.language) })}</p>
      )}
      {!hasEdit && imageInfo && "failed" in imageInfo && item?.image && (
        <p className="muted small">{t("viewer.imageFailed", { format: IMAGE_FORMATS[item.image] ?? item.image })}</p>
      )}
      {editable && (value || withheld) && (
        <>
          {binary || withheld ? (
            preview
          ) : (
            <>
              <div className="viewer-value-heading">{t("viewer.editValue")}</div>
              <textarea
                className={`cell-viewer-value cell-viewer-editor ${picker ? "with-picker" : ""}`}
                aria-label={t("edit.cellLabel", { column: meta.name })}
                spellCheck={false}
                value={value?.text ?? ""}
                placeholder={value?.text === null ? (props.defaults?.has(editKey) ? "DEFAULT" : "NULL") : undefined}
                onChange={(e) => { setCopied(false); setCopyError(null); (props.onType ?? props.onEdit)?.(row, column, e.target.value === item?.text ? undefined : e.target.value); }}
              />
              {picker && props.pickerSource && value && (
                <ValuePicker
                  key={`${resultId}:${row}:${column}`}
                  picker={picker}
                  source={props.pickerSource}
                  column={meta.name}
                  text={value.text}
                  columns={props.columns}
                  rowText={rowText}
                  onPick={applyPicked}
                />
              )}
            </>
          )}
          {binary && shown?.kind === "null" && <p className="muted cell-viewer-null">NULL</p>}
          <footer className="viewer-edit-actions" role="toolbar" aria-label={t("viewer.editActions")}>
            <span className="muted small">{t(hasEdit ? "viewer.pendingEdit" : binary ? "viewer.binaryHint" : "viewer.editHint")}</span>
            {canLoad && (
              <button
                className="ghost small"
                title={props.pendingChanges ? t("edit.applyFirst") : t("viewer.loadHint")}
                disabled={loadBlocked}
                onClick={() => void loadFromFile()}
              >
                {loadingFile ? t("viewer.loadingFile") : t("viewer.load")}
              </button>
            )}
            <button className="ghost small" title={t("edit.setNullHint")} disabled={!item || (hasEdit ? editText === null : item.kind === "null")} onClick={() => props.onEdit?.(row, column, item?.kind === "null" ? undefined : null)}>{t("edit.setNull")}</button>
          </footer>
        </>
      )}
      {!editable && shown?.kind === "null" && <p className="muted cell-viewer-null">NULL</p>}
      {!editable && preview}
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
