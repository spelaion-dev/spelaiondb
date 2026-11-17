// Settings → Snippets (SDB-384): the user's own editor snippets and the template of new
// scripts. The page only edits text and shows what the core answers: the core stores them and
// decides what they may hold (bounded sizes, no hidden characters).

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { BUILT_IN_SNIPPETS } from "../editorSnippets";
import { useSnippets } from "../snippetStore";

interface Draft {
  /** The snippet being edited, by its saved name; `null` for a new one. */
  editing: string | null;
  name: string;
  description: string;
  body: string;
}

const EMPTY: Draft = { editing: null, name: "", description: "", body: "" };

export function SnippetSettings() {
  const { t } = useTranslation("settings");
  const { t: te } = useTranslation("editor");
  const snippets = useSnippets((s) => s.snippets);
  const template = useSnippets((s) => s.template);
  const limits = useSnippets((s) => s.limits);
  const loadError = useSnippets((s) => s.loadError);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [failure, setFailure] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [templateDraft, setTemplateDraft] = useState(template);
  const [templateFailure, setTemplateFailure] = useState<string | null>(null);
  useEffect(() => setTemplateDraft(template), [template]);
  useEffect(() => void useSnippets.getState().load(), []);

  const message = (e: unknown) => (e instanceof Error ? e.message : String(e));
  const save = async () => {
    if (!draft || busy) return;
    setBusy(true);
    try {
      const store = useSnippets.getState();
      await store.save({ name: draft.name.trim(), description: draft.description, body: draft.body });
      // Renamed: the snippet under its old name goes once the new one is saved.
      if (draft.editing !== null && draft.editing !== draft.name.trim()) await store.remove(draft.editing);
      setDraft(null);
      setFailure(null);
    } catch (e) {
      setFailure(message(e));
    } finally {
      setBusy(false);
    }
  };
  const remove = async (name: string) => {
    setBusy(true);
    try {
      await useSnippets.getState().remove(name);
      if (draft?.editing === name) setDraft(null);
      setFailure(null);
    } catch (e) {
      setFailure(message(e));
    } finally {
      setBusy(false);
    }
  };
  const saveTemplate = async () => {
    try {
      await useSnippets.getState().saveTemplate(templateDraft);
      setTemplateFailure(null);
    } catch (e) {
      setTemplateFailure(message(e));
    }
  };
  const overridden = new Set(snippets.map((snippet) => snippet.name));
  const full = limits !== null && snippets.length >= limits.snippets;

  return (
    <>
      <p className="muted small">{t("snippets.hint")}</p>
      {loadError && <p className="notice error" role="alert">{t("snippets.loadFailed", { reason: loadError })}</p>}
      <fieldset className="settings-choice">
        <legend>{t("snippets.yours")}</legend>
        {snippets.length === 0 ? (
          <p className="muted small">{t("snippets.none")}</p>
        ) : (
          <ul className="snippet-list" aria-label={t("snippets.yours")}>
            {snippets.map((snippet) => (
              <li key={snippet.name}>
                <code className="snippet-name">{snippet.name}</code>
                <span className="snippet-description">{snippet.description || snippet.body.split("\n")[0]}</span>
                {BUILT_IN_SNIPPETS.some((builtIn) => builtIn.name === snippet.name) && <span className="muted small">{t("snippets.replaces")}</span>}
                <button type="button" className="small" disabled={busy} aria-label={t("snippets.editNamed", { name: snippet.name })} onClick={() => { setFailure(null); setDraft({ editing: snippet.name, ...snippet }); }}>{t("snippets.edit")}</button>
                <button type="button" className="small" disabled={busy} aria-label={t("snippets.deleteNamed", { name: snippet.name })} onClick={() => void remove(snippet.name)}>{t("snippets.delete")}</button>
              </li>
            ))}
          </ul>
        )}
        {!draft && (
          <div>
            <button type="button" className="small" disabled={busy || full} title={full ? t("snippets.full", { count: limits?.snippets ?? 0 }) : undefined} onClick={() => { setFailure(null); setDraft(EMPTY); }}>{t("snippets.add")}</button>
          </div>
        )}
        {draft && (
          <form
            className="snippet-form"
            aria-label={draft.editing === null ? t("snippets.add") : t("snippets.editNamed", { name: draft.editing })}
            onSubmit={(e) => { e.preventDefault(); void save(); }}
          >
            <label className="field">
              <span>{t("snippets.name")}</span>
              <input name="snippetName" value={draft.name} maxLength={limits?.name} spellCheck={false} autoFocus onChange={(e) => setDraft({ ...draft, name: e.target.value })} />
              <span className="muted small">{t("snippets.nameHint")}</span>
            </label>
            <label className="field">
              <span>{t("snippets.description")}</span>
              <input name="snippetDescription" value={draft.description} maxLength={limits?.description} onChange={(e) => setDraft({ ...draft, description: e.target.value })} />
            </label>
            <label className="field">
              <span>{t("snippets.text")}</span>
              <textarea name="snippetBody" className="snippet-text" rows={6} value={draft.body} spellCheck={false} onChange={(e) => setDraft({ ...draft, body: e.target.value })} />
              <span className="muted small">{t("snippets.textHint")}</span>
            </label>
            {failure && <p className="notice error" role="alert">{failure}</p>}
            <div className="snippet-actions">
              <button type="submit" className="primary small" disabled={busy || draft.name.trim() === "" || draft.body.trim() === ""}>{t("snippets.save")}</button>
              <button type="button" className="small" disabled={busy} onClick={() => { setDraft(null); setFailure(null); }}>{t("snippets.cancel")}</button>
            </div>
          </form>
        )}
        {!draft && failure && <p className="notice error" role="alert">{failure}</p>}
      </fieldset>
      <fieldset className="settings-choice">
        <legend>{t("snippets.builtIn")}</legend>
        <ul className="snippet-list" aria-label={t("snippets.builtIn")}>
          {BUILT_IN_SNIPPETS.map((snippet) => (
            <li key={snippet.name}>
              <code className="snippet-name">{snippet.name}</code>
              <span className="snippet-description">{te(snippet.description)}</span>
              {overridden.has(snippet.name) && <span className="muted small">{t("snippets.replaced")}</span>}
            </li>
          ))}
        </ul>
      </fieldset>
      <fieldset className="settings-choice">
        <legend>{t("snippets.template")}</legend>
        <label className="field">
          <span>{t("snippets.templateLabel")}</span>
          <textarea name="scriptTemplate" className="snippet-text" rows={5} value={templateDraft} spellCheck={false} onChange={(e) => setTemplateDraft(e.target.value)} onBlur={() => { if (templateDraft !== template) void saveTemplate(); }} />
          <span className="muted small">{t("snippets.templateHint")}</span>
        </label>
        {templateFailure && <p className="notice error" role="alert">{templateFailure}</p>}
        <div className="snippet-actions">
          {template !== "" && <button type="button" className="small" onClick={() => { setTemplateDraft(""); void useSnippets.getState().saveTemplate("").then(() => setTemplateFailure(null), (e: unknown) => setTemplateFailure(message(e))); }}>{t("snippets.removeTemplate")}</button>}
        </div>
      </fieldset>
    </>
  );
}
