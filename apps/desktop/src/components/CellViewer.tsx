// Cell viewer: the full value of the active cell beside the grid. JSON is pretty-printed,
// binary values become a hex dump and long text keeps its line breaks. The value is asked
// to the core, so it works for rows that are not in the grid's cache.

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

export function CellViewer(props: { resultId: number; row: number; column: number; meta: ColumnItem; onClose(): void }) {
  const { resultId, row, column, meta } = props;
  const { t, i18n } = useTranslation("editor");
  // Each answer remembers its cell, so moving to another cell never shows the previous
  // value with the new column's type while the new one loads.
  const cell = `${resultId}:${row}:${column}`;
  const [loaded, setLoaded] = useState<{ cell: string; text: string | null; error: string | null } | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let active = true;
    setCopied(false);
    call(commands.cellValue(resultId, row, column))
      .then((text) => active && setLoaded({ cell, text, error: null }))
      .catch((e: unknown) => active && setLoaded({ cell, text: null, error: e instanceof Error ? e.message : String(e) }));
    return () => {
      active = false;
    };
  }, [cell, resultId, row, column]);

  const current = loaded?.cell === cell ? loaded : null;
  const error = current?.error ?? null;
  const value = current && !current.error ? { text: current.text } : null;
  const shown = value ? view(value.text, meta) : null;
  const length =
    shown?.kind === "hex"
      ? t("viewer.bytes", { count: shown.bytes, formattedCount: shown.bytes.toLocaleString(i18n.language) })
      : value?.text != null
        ? t("viewer.characters", { count: [...value.text].length, formattedCount: [...value.text].length.toLocaleString(i18n.language) })
        : "";

  return (
    <aside className="cell-viewer" aria-label={t("viewer.title")}>
      <header>
        <div className="cell-viewer-title">
          <strong>{meta.name}</strong>
          <span className="muted small">
            {meta.nativeType} · {t("viewer.row", { number: (row + 1).toLocaleString(i18n.language) })}
            {length ? ` · ${length}` : ""}
          </span>
        </div>
        <button
          className="ghost small"
          disabled={!value || value.text === null}
          onClick={() => {
            if (value?.text != null) void navigator.clipboard.writeText(value.text).then(() => setCopied(true));
          }}
        >
          {copied ? t("viewer.copied") : t("viewer.copy")}
        </button>
        <button className="ghost" aria-label={t("viewer.close")} onClick={props.onClose}>
          ×
        </button>
      </header>
      {error && <p className="error small" role="alert">{error}</p>}
      {!value && !error && <p className="muted small">{t("viewer.loading")}</p>}
      {shown?.kind === "null" && <p className="muted cell-viewer-null">NULL</p>}
      {shown && shown.kind !== "null" && (
        <pre className={`cell-viewer-value cell-viewer-${shown.kind}`} tabIndex={0}>
          {shown.text}
        </pre>
      )}
    </aside>
  );
}
