// References panel of a table data tab: for the active row, the row each of its foreign keys
// points to, and the first rows of other tables whose keys point to it. The core finds the
// keys in the catalog, reads the values from the stored row and writes each filter; "Open"
// opens a table data tab with that filter, so a key can be followed again from there.

import { useEffect, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type KeyTargetItem, type RowReferenceItem } from "../api";
import { Icon } from "./Icons";

/** Rows of each key the panel shows; "Open" shows them all. */
export const REFERENCE_PREVIEW_ROWS = 5;

/** How long the active row must stay before its references are read (arrow keys go fast). */
const SETTLE_MS = 200;

/** Columns shown in one line for a referencing row; its tooltip has them all. */
const SUMMARY_COLUMNS = 4;

export function ReferencesPanel(props: {
  session: string;
  resultId: number;
  row: number;
  schema: string;
  table: string;
  modes: ReactNode;
  onOpen(target: KeyTargetItem): void;
  onClose(): void;
}) {
  const { session, resultId, row, schema, table } = props;
  const { t, i18n } = useTranslation("editor");
  // Each answer remembers its row, so moving to another row never shows the previous one's.
  const key = `${resultId}:${row}`;
  const [loaded, setLoaded] = useState<{ key: string; items: RowReferenceItem[] } | { key: string; error: string } | null>(null);

  useEffect(() => {
    let active = true;
    const timer = setTimeout(() => {
      call(commands.rowReferences(session, resultId, row, schema, table, REFERENCE_PREVIEW_ROWS))
        .then((items) => active && setLoaded({ key, items }))
        .catch((e: unknown) => active && setLoaded({ key, error: e instanceof Error ? e.message : String(e) }));
    }, SETTLE_MS);
    return () => {
      active = false;
      clearTimeout(timer);
    };
  }, [key, session, resultId, row, schema, table]);

  const current = loaded?.key === key ? loaded : null;
  const items = current && "items" in current ? current.items : [];
  const outgoing = items.filter((item) => item.key.direction === "outgoing");
  const incoming = items.filter((item) => item.key.direction === "incoming");
  const name = (item: RowReferenceItem) => (item.key.schema === schema ? item.key.table : `${item.key.schema}.${item.key.table}`);

  const block = (item: RowReferenceItem) => {
    const out = item.key.direction === "outgoing";
    const columns = out
      ? `${item.key.columns.join(", ")} → ${item.key.otherColumns.join(", ")}`
      : `${item.key.otherColumns.join(", ")} → ${item.key.columns.join(", ")}`;
    const target = item.target;
    return (
      <li key={`${item.key.direction}:${item.key.schema}.${item.key.table}:${item.key.name}`} className="reference-key">
        <div className="reference-key-heading">
          <div className="reference-key-title">
            <span className="reference-table" title={`${item.key.schema}.${item.key.table}`}>{name(item)}</span>
            <span className="reference-columns" title={`${item.key.name}: ${columns}`}>{columns}</span>
          </div>
          {target && item.rows.length > 0 && (
            <button
              type="button"
              className="ghost small"
              title={t(out ? "references.openRowHint" : "references.openRowsHint", { table: name(item) })}
              onClick={() => props.onOpen(target)}
            >
              {t(out ? "references.openRow" : "references.openRows")}
            </button>
          )}
        </div>
        {item.nullColumn != null ? (
          <p className="reference-note">{t("references.nullKey", { column: item.nullColumn })}</p>
        ) : item.error != null ? (
          <p className="error small" role="alert">{t("references.failed", { detail: item.error })}</p>
        ) : item.rows.length === 0 ? (
          <p className="reference-note">{t(out ? "references.missing" : "references.noRows")}</p>
        ) : out ? (
          <dl className="reference-fields">
            {item.columns.map((column, i) => {
              const value = item.rows[0]?.[i] ?? null;
              return (
                <div key={i} className="reference-field">
                  <dt title={column}>{column}</dt>
                  <dd className={value === null ? "muted" : ""} title={value ?? "NULL"}>{value ?? "NULL"}</dd>
                </div>
              );
            })}
          </dl>
        ) : (
          <>
            <ul className="reference-rows">
              {item.rows.map((values, r) => {
                const all = item.columns.map((column, i) => `${column}: ${values[i] ?? "NULL"}`);
                return (
                  <li key={r} title={all.join("\n")}>{all.slice(0, SUMMARY_COLUMNS).join(" · ")}</li>
                );
              })}
            </ul>
            {item.more && <p className="reference-note">{t("references.more", { count: item.rows.length })}</p>}
          </>
        )}
      </li>
    );
  };

  return (
    <aside className="cell-viewer references-panel" aria-label={t("references.title")} onKeyDown={(e) => e.key === "Escape" && props.onClose()}>
      <header>
        <div className="cell-viewer-heading">
          <div className="cell-viewer-title">
            <strong>{t("references.rowTitle", { number: (row + 1).toLocaleString(i18n.language) })}</strong>
            <span className="muted small" title={`${schema}.${table}`}>{table}</span>
          </div>
          <button className="ghost cell-viewer-close" aria-label={t("references.close")} title={t("references.close")} onClick={props.onClose}><Icon name="x" /></button>
        </div>
        <div className="cell-viewer-controls">{props.modes}</div>
      </header>
      <div className="references-body" aria-busy={!current}>
        {!current && <p className="reference-note">{t("references.loading")}</p>}
        {current && "error" in current && <p className="error small" role="alert">{t("references.failed", { detail: current.error })}</p>}
        {current && "items" in current && items.length === 0 && <p className="reference-note">{t("references.none")}</p>}
        {outgoing.length > 0 && (
          <section aria-label={t("references.outgoing")}>
            <h3 className="reference-section">{t("references.outgoing")}</h3>
            <ul className="reference-keys">{outgoing.map(block)}</ul>
          </section>
        )}
        {incoming.length > 0 && (
          <section aria-label={t("references.incoming")}>
            <h3 className="reference-section">{t("references.incoming")}</h3>
            <ul className="reference-keys">{incoming.map(block)}</ul>
          </section>
        )}
      </div>
    </aside>
  );
}
