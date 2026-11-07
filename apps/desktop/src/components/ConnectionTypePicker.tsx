// The connection form's type picker (ADR-029): a connection type by id, or a plain label of
// the user's own. The types and what they do come from the core.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ConnectionTypeItem } from "../api";
import { TypeBadge } from "./TypeBadge";

/** Every connection type, as the core resolved them; empty until loaded. */
export function useConnectionTypes(): ConnectionTypeItem[] {
  const [types, setTypes] = useState<ConnectionTypeItem[]>([]);
  useEffect(() => {
    call(commands.listConnectionTypes())
      .then(setTypes)
      .catch(() => setTypes([]));
  }, []);
  return types;
}

const LABEL = "\u0000label";

/**
 * The type the form's `env` names: a type id picked here, or, for the label the connection
 * already had (`PROD`, `production`…), the type the core resolved it to.
 */
export function pickedType(types: ConnectionTypeItem[], env: string, saved: { env: string; kind: ConnectionTypeItem | null } | null): ConnectionTypeItem | null {
  return types.find((t) => t.id === env) ?? (saved && env === saved.env ? saved.kind : null);
}

export function ConnectionTypePicker(props: {
  types: ConnectionTypeItem[];
  env: string;
  /** The edited connection's label and its type, as saved. */
  saved: { env: string; kind: ConnectionTypeItem | null } | null;
  onChange(env: string): void;
}) {
  const { t } = useTranslation("connections");
  const { types, env, saved, onChange } = props;
  const picked = pickedType(types, env, saved);
  const [labelMode, setLabelMode] = useState(env.trim() !== "" && picked === null);
  const value = picked ? picked.id : labelMode ? LABEL : "";
  return (
    <div className="type-picker">
      <div className="form-row">
        {picked && <TypeBadge type={picked} small />}
        <select
          aria-label={t("fields.env")}
          value={value}
          onChange={(e) => {
            const next = e.target.value;
            setLabelMode(next === LABEL);
            if (next === LABEL) onChange(picked ? "" : env);
            else onChange(next);
          }}
        >
          <option value="">{t("fields.noType")}</option>
          {types.map((type) => (
            <option key={type.id} value={type.id}>
              {type.name}
            </option>
          ))}
          <option value={LABEL}>{t("fields.customLabel")}</option>
        </select>
      </div>
      {value === LABEL && (
        <input type="text" aria-label={t("fields.label")} value={env} placeholder="staging, qa…" onChange={(e) => onChange(e.target.value)} />
      )}
      {picked && <p className="muted small">{describe(picked, t)}</p>}
      {value === LABEL && <p className="muted small">{t("fields.labelHint")}</p>}
    </div>
  );
}

function describe(type: ConnectionTypeItem, t: (key: string, options?: Record<string, unknown>) => string): string {
  const s = type.safeguards;
  const parts = [
    s.confirmExecution ? t("fields.typeConfirmsExecution") : null,
    s.confirmDataChanges ? t("fields.typeConfirmsChanges") : null,
    s.closeIdleAfter ? t("fields.typeClosesIdle", { minutes: Math.max(1, Math.round(s.closeIdleAfter / 60)) }) : null,
  ].filter((p): p is string => p !== null);
  return parts.length > 0 ? parts.join(" · ") : t("fields.typeNoSafeguards");
}
