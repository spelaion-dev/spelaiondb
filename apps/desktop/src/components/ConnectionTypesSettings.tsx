// Settings page for connection types (ADR-029): the built-in, team and personal types with every
// layer applied, as the core resolved them. Saving writes only the user's own definition; the
// core keeps every safeguard another layer turns on, so the form shows those as fixed.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type ConnectionTypeDraft, type ConnectionTypeItem } from "../api";
import type { ToneItem } from "../bindings";
import { refreshConnections } from "./Sidebar";
import { TypeBadge } from "./TypeBadge";

/** The badge palette, in the order the picker offers it (ADR-029). */
const TONES: ToneItem[] = ["neutral", "green", "blue", "amber", "red"];

interface Editing {
  id: string;
  isNew: boolean;
  /** The type as the core resolved it; null for a new one. */
  base: ConnectionTypeItem | null;
  draft: ConnectionTypeDraft;
}

const EMPTY: ConnectionTypeDraft = {
  name: "",
  tone: null,
  description: "",
  confirmExecution: false,
  confirmDataChanges: false,
  closeIdleAfter: null,
};

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function ConnectionTypesSettings() {
  const { t } = useTranslation("settings");
  const [types, setTypes] = useState<ConnectionTypeItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [editing, setEditing] = useState<Editing | null>(null);

  function refresh() {
    call(commands.listConnectionTypes())
      .then((list) => {
        setTypes(list);
        setError(null);
      })
      .catch((e: unknown) => setError(message(e)));
  }
  useEffect(refresh, []);

  async function changed() {
    setEditing(null);
    refresh();
    // The sidebar and the tabs show the new colors.
    await refreshConnections().catch(() => undefined);
  }

  async function remove(type: ConnectionTypeItem) {
    try {
      await call(commands.deleteConnectionType(type.id));
      await changed();
    } catch (e) {
      setError(message(e));
    }
  }

  return (
    <section aria-label={t("types.title")}>
      <p className="muted small">{t("types.intro")}</p>
      {error && <p className="error small" role="alert">{error}</p>}
      {types && (
        <ul className="profile-list type-list">
          {types.map((type) => (
            <li key={type.id} className="profile-row" aria-label={type.name} data-type-id={type.id}>
              <div className="grow">
                <TypeBadge type={type} small /> <span className="mono small muted">{type.id}</span>{" "}
                <span className="muted small">{origin(type, t)}</span>
                {type.description && <div className="small">{type.description}</div>}
                <div className="muted small">{safeguardsText(type, t)}</div>
              </div>
              <button onClick={() => setEditing({ id: type.id, isNew: false, base: type, draft: type.personal ?? EMPTY })}>{t("types.edit")}</button>
              {type.personal && (
                <button className="ghost" onClick={() => void remove(type)}>
                  {type.builtIn || type.team ? t("types.reset") : t("types.delete")}
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
      <button onClick={() => setEditing({ id: "", isNew: true, base: null, draft: { ...EMPTY, tone: "blue" } })}>{t("types.new")}</button>
      {editing && <TypeEditor editing={editing} onCancel={() => setEditing(null)} onSaved={() => void changed()} />}
    </section>
  );
}

function origin(type: ConnectionTypeItem, t: (key: string) => string): string {
  const parts = [type.builtIn ? t("types.builtIn") : type.team ? t("types.team") : t("types.personal")];
  if ((type.builtIn || type.team) && type.personal) parts.push(t("types.changedByYou"));
  return parts.join(" · ");
}

function safeguardsText(type: ConnectionTypeItem, t: (key: string, options?: Record<string, unknown>) => string): string {
  const s = type.safeguards;
  const parts = [
    s.confirmExecution ? t("types.confirmExecution") : null,
    s.confirmDataChanges ? t("types.confirmDataChanges") : null,
    s.closeIdleAfter ? t("types.closesAfter", { minutes: Math.max(1, Math.round(s.closeIdleAfter / 60)) }) : null,
  ].filter((p): p is string => p !== null);
  return parts.length > 0 ? parts.join(" · ") : t("types.noSafeguards");
}

function TypeEditor({ editing, onCancel, onSaved }: { editing: Editing; onCancel(): void; onSaved(): void }) {
  const { t } = useTranslation("settings");
  const [id, setId] = useState(editing.id);
  const [draft, setDraft] = useState<ConnectionTypeDraft>(editing.draft);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const base = editing.base;
  const inherited = base?.inherited;
  const update = (patch: Partial<ConnectionTypeDraft>) => setDraft((d) => ({ ...d, ...patch }));
  // What the badge looks like with this draft: another layer's tone when it leaves it empty.
  const shownTone: ToneItem = draft.tone ?? base?.tone ?? "neutral";
  const minutes = draft.closeIdleAfter ? String(Math.round(draft.closeIdleAfter / 60)) : "";

  async function save() {
    setSaving(true);
    setError(null);
    try {
      await call(commands.saveConnectionType(id.trim(), draft));
      onSaved();
    } catch (e) {
      setError(message(e));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <form
        className="modal type-editor"
        role="dialog"
        aria-modal="true"
        aria-label={editing.isNew ? t("types.newTitle") : t("types.editTitle", { name: base?.name ?? id })}
        onSubmit={(e) => {
          e.preventDefault();
          if (!saving) void save();
        }}
        onKeyDown={(e) => e.key === "Escape" && onCancel()}
      >
        <h3>{editing.isNew ? t("types.newTitle") : t("types.editTitle", { name: base?.name ?? id })}</h3>
        {!editing.isNew && (base?.builtIn || base?.team) && <p className="muted small">{t("types.overrideHint")}</p>}
        <div className="form-pair">
          <label className="field grow">
            <span>{t("types.id")}</span>
            <input type="text" value={id} disabled={!editing.isNew} placeholder="staging" onChange={(e) => setId(e.target.value.toLowerCase())} autoFocus={editing.isNew} />
          </label>
          <label className="field grow">
            <span>{t("types.name")}</span>
            <input type="text" value={draft.name} placeholder={base?.name ?? ""} maxLength={40} onChange={(e) => update({ name: e.target.value })} autoFocus={!editing.isNew} />
          </label>
        </div>
        <div className="form-pair">
          <label className="field grow">
            <span>{t("types.tone")}</span>
            <select aria-label={t("types.tone")} value={draft.tone ?? ""} onChange={(e) => update({ tone: e.target.value === "" ? null : (e.target.value as ToneItem) })}>
              {!editing.isNew && <option value="">{t("types.toneDefault", { tone: t(`types.tones.${base?.tone ?? "neutral"}`) })}</option>}
              {TONES.map((tone) => (
                <option key={tone} value={tone}>
                  {t(`types.tones.${tone}`)}
                </option>
              ))}
            </select>
          </label>
          <div className="field grow">
            <span>{t("types.preview")}</span>
            <div className="form-row">
              <TypeBadge type={{ id: id || "new", name: draft.name.trim() || base?.name || id || "…", tone: shownTone, production: false, description: null }} />
            </div>
          </div>
        </div>
        <label className="field">
          <span>{t("types.description")}</span>
          <input type="text" value={draft.description} placeholder={base?.description ?? ""} maxLength={200} onChange={(e) => update({ description: e.target.value })} />
        </label>
        <fieldset>
          <legend>{t("types.safeguards")}</legend>
          <label className="check">
            <input
              type="checkbox"
              checked={draft.confirmExecution || Boolean(inherited?.confirmExecution)}
              disabled={Boolean(inherited?.confirmExecution)}
              onChange={(e) => update({ confirmExecution: e.target.checked })}
            />
            {t("types.confirmExecution")}
          </label>
          <label className="check">
            <input
              type="checkbox"
              checked={draft.confirmDataChanges || Boolean(inherited?.confirmDataChanges)}
              disabled={Boolean(inherited?.confirmDataChanges)}
              onChange={(e) => update({ confirmDataChanges: e.target.checked })}
            />
            {t("types.confirmDataChanges")}
          </label>
          <label className="field port">
            <span>{t("types.closeIdle")}</span>
            <input
              type="number"
              min={0}
              max={1440}
              value={minutes}
              placeholder={inherited?.closeIdleAfter ? String(Math.round(inherited.closeIdleAfter / 60)) : t("types.never")}
              onChange={(e) => update({ closeIdleAfter: e.target.value ? Math.max(0, Math.trunc(Number(e.target.value))) * 60 : null })}
            />
          </label>
          <p className="muted small">{t("types.closeIdleHint")}</p>
          {(inherited?.confirmExecution || inherited?.confirmDataChanges || inherited?.closeIdleAfter) && (
            <p className="muted small">{t("types.inheritedHint")}</p>
          )}
        </fieldset>
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          <button type="button" onClick={onCancel}>
            {t("types.cancel")}
          </button>
          <button type="submit" className="primary" disabled={saving || (editing.isNew && id.trim() === "")}>
            {t("types.save")}
          </button>
        </div>
      </form>
    </div>
  );
}
