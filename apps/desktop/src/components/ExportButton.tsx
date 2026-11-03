// "Export" menu of a result: the core writes the whole result to a file chosen in the native
// save dialog (CSV, TSV, JSON or SQL INSERT), without passing the rows through the UI.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type TextFormat } from "../api";
import { chooseSavePath } from "../dialogs";
import type { ResultMeta } from "../store";

const FORMATS: { format: TextFormat; extension: string }[] = [
  { format: "csv", extension: "csv" },
  { format: "tsv", extension: "tsv" },
  { format: "json", extension: "json" },
  { format: "sqlInsert", extension: "sql" },
  { format: "markdown", extension: "md" },
];

export function ExportButton({ result, session }: { result: ResultMeta; session: string }) {
  const { t, i18n } = useTranslation("editor");
  // Where the menu opens: fixed to the window, because the result tab bar clips overflow.
  const [open, setOpen] = useState<{ top: number; right: number } | null>(null);
  const [busy, setBusy] = useState(false);
  const [notice, setNotice] = useState<{ text: string; error: boolean } | null>(null);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && root.current?.contains(e.target as Node)) return;
      setOpen(null);
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [open]);

  async function exportAs(format: TextFormat, extension: string) {
    setOpen(null);
    const path = await chooseSavePath({
      defaultPath: `result.${extension}`,
      filters: [{ name: t(`results.export.${format}`), extensions: [extension] }],
    });
    if (!path) return;
    setBusy(true);
    setNotice(null);
    try {
      const rows = (await call(commands.exportResult(result.id, session, path, format))) ?? 0;
      setNotice({
        text: t("results.export.done", { count: rows, formattedCount: rows.toLocaleString(i18n.language), path }),
        error: false,
      });
    } catch (e) {
      setNotice({ text: e instanceof Error ? e.message : String(e), error: true });
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="export-button" ref={root}>
      {notice && (
        <span className={`small ${notice.error ? "error" : "muted"}`} role="status" title={notice.text}>
          {notice.text}
        </span>
      )}
      <button
        aria-haspopup="true"
        aria-expanded={open !== null}
        disabled={busy || !result.complete}
        title={result.complete ? t("results.export.hint") : t("results.export.loading")}
        onClick={(e) => {
          const rect = e.currentTarget.getBoundingClientRect();
          setOpen(open ? null : { top: rect.bottom + 2, right: window.innerWidth - rect.right });
        }}
      >
        {busy ? t("results.export.busy") : t("results.export.button")}
      </button>
      {open && (
        <div className="grid-menu export-menu" role="menu" style={open}>
          {FORMATS.map(({ format, extension }) => (
            <button key={format} role="menuitem" onClick={() => void exportAs(format, extension)}>
              {t(`results.export.${format}`)}
            </button>
          ))}
        </div>
      )}
    </div>
  );
}
