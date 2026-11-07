// "Export…" of a result: opens the export dialog (ExportDialog.tsx), where the user picks the
// loaded rows or every row (the query run again in a new session, ADR-034), the format and the
// CSV options. The core writes the file without passing the rows through the UI.

import { useTranslation } from "react-i18next";
import { useExport } from "./ExportDialog";
import type { ResultMeta } from "../store";

export function ExportButton({
  result,
  session,
  connectionId,
  sql,
  table,
  hidden,
}: {
  result: ResultMeta;
  session: string;
  connectionId: string | null;
  /** The statement that produced the result, to run it again; null when unknown. */
  sql: string | null;
  table: { schema: string; name: string; filter: string; orderBy: string } | null;
  hidden: number[];
}) {
  const { t } = useTranslation("editor");
  const open = useExport((s) => s.open);
  return (
    <div className="export-button">
      <button
        title={t("results.export.hint")}
        onClick={() => open({ kind: "result", session, connectionId, result, sql, table, hidden })}
      >
        {t("results.export.button")}
      </button>
    </div>
  );
}
