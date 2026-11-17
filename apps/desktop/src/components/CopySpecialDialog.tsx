import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import type { CopySpecialItem, RowDelimiterItem } from "../bindings";

/** The delimiters offered by name; anything else is typed as "Other". */
const DELIMITERS: { value: string; key: string }[] = [
  { value: "\t", key: "tab" },
  { value: ",", key: "comma" },
  { value: ";", key: "semicolon" },
  { value: "|", key: "pipe" },
  { value: " ", key: "space" },
];
const OTHER = "other";
const QUOTES: { value: string; key: string }[] = [
  { value: "\"", key: "double" },
  { value: "'", key: "single" },
  { value: "", key: "none" },
];

/** "Copy special…" (SDB-388): the selection as delimited text, written by the core with the
 *  options chosen here, which the core keeps as the copy defaults. The values are the
 *  database's own text, never the grid's display formats; the core checks every option. */
export function CopySpecialDialog(props: {
  /** The copy defaults, read from the core when the dialog opens. */
  load(): Promise<CopySpecialItem>;
  /** Copies with these options; a refusal is thrown and shown in the dialog. */
  copy(options: CopySpecialItem): Promise<void>;
  onClose(): void;
}) {
  const { t } = useTranslation("editor");
  const [options, setOptions] = useState<CopySpecialItem | null>(null);
  const [other, setOther] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const { load } = props;
  useEffect(() => {
    let live = true;
    load()
      .then((loaded) => {
        if (!live) return;
        setOptions(loaded);
        setOther(!DELIMITERS.some((d) => d.value === loaded.delimiter));
      })
      .catch((e: unknown) => { if (live) setError(e instanceof Error ? e.message : String(e)); });
    return () => { live = false; };
  }, [load]);

  const change = (patch: Partial<CopySpecialItem>) => {
    setError(null);
    setOptions((current) => (current ? { ...current, ...patch } : current));
  };
  async function submit() {
    if (!options || busy) return;
    setBusy(true);
    setError(null);
    try {
      await props.copy(options);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <form
        className="modal copy-special"
        role="dialog"
        aria-modal="true"
        aria-label={t("results.copySpecialTitle")}
        onSubmit={(e) => { e.preventDefault(); void submit(); }}
        onKeyDown={(e) => {
          if (e.key === "Escape" && !busy) { e.stopPropagation(); props.onClose(); }
        }}
      >
        <h2>{t("results.copySpecialTitle")}</h2>
        <p className="muted small">{t("results.copySpecialHint")}</p>
        {options && (
          <div className="copy-special-fields">
            <label className="field">
              <span>{t("results.copyDelimiter")}</span>
              <select
                autoFocus
                aria-label={t("results.copyDelimiter")}
                value={other ? OTHER : options.delimiter}
                onChange={(e) => {
                  if (e.target.value === OTHER) { setOther(true); change({ delimiter: "" }); }
                  else { setOther(false); change({ delimiter: e.target.value }); }
                }}
              >
                {DELIMITERS.map((d) => <option key={d.key} value={d.value}>{t(`results.copyDelimiters.${d.key}`)}</option>)}
                <option value={OTHER}>{t("results.copyDelimiters.other")}</option>
              </select>
            </label>
            {other && (
              <label className="field">
                <span>{t("results.copyOtherDelimiter")}</span>
                <input
                  type="text"
                  className="copy-special-char"
                  aria-label={t("results.copyOtherDelimiter")}
                  autoFocus
                  maxLength={2}
                  spellCheck={false}
                  value={options.delimiter}
                  onChange={(e) => change({ delimiter: e.target.value })}
                />
              </label>
            )}
            <label className="field">
              <span>{t("results.copyQuote")}</span>
              <select aria-label={t("results.copyQuote")} value={options.quote} onChange={(e) => change({ quote: e.target.value })}>
                {QUOTES.map((q) => <option key={q.key} value={q.value}>{t(`results.copyQuotes.${q.key}`)}</option>)}
              </select>
            </label>
            <label className="field">
              <span>{t("results.copyRowDelimiter")}</span>
              <select aria-label={t("results.copyRowDelimiter")} value={options.rowDelimiter} onChange={(e) => change({ rowDelimiter: e.target.value as RowDelimiterItem })}>
                <option value="lf">{t("results.copyRowDelimiters.lf")}</option>
                <option value="crLf">{t("results.copyRowDelimiters.crLf")}</option>
              </select>
            </label>
            <label className="field">
              <span>{t("results.copyNullText")}</span>
              <input
                type="text"
                aria-label={t("results.copyNullText")}
                spellCheck={false}
                placeholder={t("results.copyNullEmpty")}
                value={options.nullText}
                onChange={(e) => change({ nullText: e.target.value })}
              />
            </label>
            <div className="copy-special-checks">
              <label className="check">
                <input type="checkbox" checked={options.header} onChange={(e) => change({ header: e.target.checked })} />
                {t("results.copyHeader")}
              </label>
              <label className="check">
                <input type="checkbox" checked={options.rowNumbers} onChange={(e) => change({ rowNumbers: e.target.checked })} />
                {t("results.copyRowNumbers")}
              </label>
            </div>
          </div>
        )}
        {error && <p className="error" role="alert">{t("results.copyFailed", { reason: error })}</p>}
        <div className="modal-actions">
          <button type="button" disabled={busy} onClick={props.onClose}>{t("results.cancel")}</button>
          <button type="submit" className="primary" disabled={!options || busy}>
            {t(busy ? "results.copying" : "results.copySubmit")}
          </button>
        </div>
      </form>
    </div>
  );
}
