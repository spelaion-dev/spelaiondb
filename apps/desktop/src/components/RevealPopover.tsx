// Revealing one masked cell (ADR-051): the core reads that cell's value from what it keeps
// aside, without running anything, and this popover shows it. The value lives only here: it is
// never drawn in the grid or kept, and closing the popover hides it again. Copying it is the
// user's own explicit act.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type RevealedCellItem } from "../api";
import { Icon } from "./Icons";

const MARGIN = 8;

export function RevealPopover(props: {
  x: number;
  y: number;
  resultId: number;
  row: number;
  column: number;
  onClose(): void;
}) {
  const { resultId, row, column, onClose } = props;
  const { t } = useTranslation("editor");
  const box = useRef<HTMLDivElement>(null);
  const [cell, setCell] = useState<RevealedCellItem | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [copied, setCopied] = useState(false);

  useEffect(() => {
    let active = true;
    call(commands.revealCell(resultId, row, column))
      .then((revealed) => active && setCell(revealed))
      .catch((e: unknown) => active && setError(e instanceof Error ? e.message : String(e)));
    return () => {
      active = false;
    };
  }, [resultId, row, column]);

  useLayoutEffect(() => {
    const el = box.current;
    if (!el) return;
    const rect = el.getBoundingClientRect();
    el.style.left = `${Math.max(MARGIN, Math.min(props.x, window.innerWidth - rect.width - MARGIN))}px`;
    el.style.top = `${Math.max(MARGIN, Math.min(props.y, window.innerHeight - rect.height - MARGIN))}px`;
  });

  useEffect(() => {
    box.current?.focus();
    const close = (e: Event) => {
      if (e instanceof KeyboardEvent && e.key !== "Escape") return;
      if (e instanceof MouseEvent && box.current?.contains(e.target as Node)) return;
      onClose();
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("keydown", close);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("keydown", close);
    };
  }, [onClose]);

  async function copy() {
    if (cell?.text === null || cell?.text === undefined) return;
    try {
      await navigator.clipboard.writeText(cell.text);
      setCopied(true);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  const title = t("masking.revealTitle", { column: cell?.column ?? "" });
  return (
    <div ref={box} className="column-stats reveal-popover" role="dialog" aria-label={title} tabIndex={-1} style={{ left: props.x, top: props.y }}>
      <header className="column-stats-header">
        <div className="column-stats-title">
          <strong title={title}>{title}</strong>
          {cell && <span className="muted small">{cell.nativeType}</span>}
        </div>
        <button className="ghost cell-viewer-close" aria-label={t("masking.close")} title={t("masking.close")} onClick={onClose}>
          <Icon name="x" />
        </button>
      </header>
      {error && <p className="error small" role="alert">{error}</p>}
      {cell && (
        <>
          {cell.text === null ? (
            <p className="reveal-value muted">{t("masking.null")}</p>
          ) : (
            <pre className="reveal-value" data-testid="revealed-value">{cell.text}</pre>
          )}
          {cell.cut && <p className="small muted reveal-note">{t("masking.revealCut")}</p>}
          <p className="small muted reveal-note">{t("masking.revealNote")}</p>
          <div className="reveal-actions">
            <button type="button" disabled={cell.text === null} onClick={() => void copy()}>
              {copied ? t("masking.copied") : t("masking.copy")}
            </button>
          </div>
        </>
      )}
    </div>
  );
}
