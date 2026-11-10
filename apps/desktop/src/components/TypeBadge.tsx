// A connection type's badge (ADR-029): its name, tinted with its tone from the fixed palette in
// styles.css (`.tone-*`), which looks right in both themes. The type comes from the core.

import { useTranslation } from "react-i18next";
import type { ConnectionTypeItem } from "../api";

export function TypeBadge({ type, small = false }: { type: Pick<ConnectionTypeItem, "id" | "name" | "tone" | "production" | "description">; small?: boolean }) {
  const { t } = useTranslation("editor");
  return (
    <span
      className={`badge type-badge tone-${type.tone}${small ? " type-badge-small" : ""}${type.production ? " badge-prod" : ""}`}
      data-connection-type={type.id}
      title={type.description ?? type.name}
    >
      {small ? (
        // Its own element so the explorer can keep the dot and drop the name (connectionMarks.ts).
        <span className="type-badge-text">{type.name}</span>
      ) : type.production ? (
        t("production")
      ) : (
        type.name
      )}
    </span>
  );
}
