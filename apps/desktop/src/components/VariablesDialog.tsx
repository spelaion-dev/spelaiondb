// The variables of an editor tab (SDB-383): the `${name}` values of this tab, the ones saved
// for its connection, and the `:name` parameter values the tab remembers. The core writes the
// values into a statement before it runs and decides what a name and a value may be; this
// only lists and edits them. The tab's values stay in the page's memory; the connection's are
// in the local store. Nothing here is sent anywhere.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import { useApp } from "../store";
import { Icon } from "./Icons";

type Scope = "tab" | "connection";

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function VariablesDialog({ tabId, connectionId, connectionName, onClose }: {
  tabId: string;
  connectionId: string | null;
  connectionName: string | null;
  onClose(): void;
}) {
  const { t } = useTranslation("editor");
  const tabVariables = useApp((s) => s.tabs.find((tab) => tab.id === tabId)?.variables) ?? {};
  const params = useApp((s) => s.tabs.find((tab) => tab.id === tabId)?.params) ?? {};
  const updateTab = useApp((s) => s.updateTab);
  const [saved, setSaved] = useState<{ name: string; value: string }[] | null>(connectionId ? null : []);
  const [error, setError] = useState<string | null>(null);
  const [name, setName] = useState("");
  const [value, setValue] = useState("");
  const [scope, setScope] = useState<Scope>("tab");
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (!connectionId) return;
    let live = true;
    call(commands.connectionVariables(connectionId))
      .then((list) => { if (live) setSaved(list); })
      .catch((e: unknown) => { if (live) { setSaved([]); setError(t("variables.loadFailed", { detail: message(e) })); } });
    return () => { live = false; };
  }, [connectionId, t]);

  const setTab = (next: Record<string, string>) => updateTab(tabId, { variables: next });

  /** Sets a variable where it lives; the core says whether the name and the value can be one. */
  async function set(where: Scope, variable: string, text: string): Promise<boolean> {
    setError(null);
    setBusy(true);
    try {
      if (!(await call(commands.variableIsValid(variable, text)))) {
        setError(t("variables.invalid"));
        return false;
      }
      if (where === "tab") {
        setTab({ ...tabVariables, [variable]: text });
      } else if (connectionId) {
        await call(commands.setConnectionVariable(connectionId, variable, text));
        setSaved(await call(commands.connectionVariables(connectionId)));
      }
      return true;
    } catch (e) {
      setError(t("variables.saveFailed", { detail: message(e) }));
      return false;
    } finally {
      setBusy(false);
    }
  }

  async function remove(where: Scope, variable: string) {
    setError(null);
    if (where === "tab") {
      const next = { ...tabVariables };
      delete next[variable];
      setTab(next);
      return;
    }
    if (!connectionId) return;
    setBusy(true);
    try {
      await call(commands.setConnectionVariable(connectionId, variable, null));
      setSaved(await call(commands.connectionVariables(connectionId)));
    } catch (e) {
      setError(t("variables.saveFailed", { detail: message(e) }));
    } finally {
      setBusy(false);
    }
  }

  const rows = (where: Scope, list: { name: string; value: string }[]) => (
    <ul className="variables-list" data-scope={where}>
      {list.map((variable) => (
        <li key={variable.name}>
          <span className="mono variables-name">{`\${${variable.name}}`}</span>
          <input
            className="mono"
            aria-label={t("variables.valueOf", { name: variable.name })}
            defaultValue={variable.value}
            key={variable.value}
            spellCheck={false}
            autoComplete="off"
            disabled={busy}
            onBlur={(e) => { if (e.target.value !== variable.value) void set(where, variable.name, e.target.value); }}
            onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); e.currentTarget.blur(); } }}
          />
          <button type="button" className="ghost" disabled={busy} aria-label={t("variables.removeNamed", { name: variable.name })} title={t("variables.remove")} onClick={() => void remove(where, variable.name)}>
            <Icon name="x" />
          </button>
        </li>
      ))}
    </ul>
  );

  const tabList = Object.entries(tabVariables).sort(([a], [b]) => a.localeCompare(b)).map(([n, v]) => ({ name: n, value: v }));
  const paramList = Object.entries(params).sort(([a], [b]) => a.localeCompare(b));
  return (
    <div className="modal-backdrop">
      <form
        className="modal variables-dialog"
        role="dialog"
        aria-modal="true"
        aria-label={t("variables.title")}
        onSubmit={(e) => {
          e.preventDefault();
          const variable = name.trim();
          if (!variable) return;
          void set(scope, variable, value).then((ok) => { if (ok) { setName(""); setValue(""); } });
        }}
        onKeyDown={(e) => {
          if (e.key === "Escape") { e.stopPropagation(); onClose(); }
        }}
      >
        <h2>{t("variables.title")}</h2>
        <p className="muted small">{t("variables.explanation")}</p>

        <h3 className="variables-heading">{t("variables.tab")}</h3>
        {tabList.length > 0 ? rows("tab", tabList) : <p className="muted small">{t("variables.noneTab")}</p>}

        <h3 className="variables-heading">{connectionName ? t("variables.connection", { name: connectionName }) : t("variables.connectionNone")}</h3>
        {saved === null
          ? <p className="muted small">{t("variables.loading")}</p>
          : saved.length > 0 ? rows("connection", saved) : <p className="muted small">{connectionId ? t("variables.noneConnection") : t("variables.pickConnection")}</p>}

        <fieldset className="variables-add">
          <legend>{t("variables.add")}</legend>
          <label className="field">
            <span>{t("variables.name")}</span>
            <input className="mono" name="variableName" autoFocus value={name} onChange={(e) => setName(e.target.value)} spellCheck={false} autoComplete="off" maxLength={64} />
          </label>
          <label className="field">
            <span>{t("variables.value")}</span>
            <input className="mono" name="variableValue" value={value} onChange={(e) => setValue(e.target.value)} spellCheck={false} autoComplete="off" />
          </label>
          <label className="field">
            <span>{t("variables.keptIn")}</span>
            <select name="variableScope" value={scope} onChange={(e) => setScope(e.target.value as Scope)}>
              <option value="tab">{t("variables.scopeTab")}</option>
              <option value="connection" disabled={!connectionId}>{t("variables.scopeConnection")}</option>
            </select>
          </label>
          <button type="submit" disabled={busy || !name.trim()}>{t("variables.addButton")}</button>
        </fieldset>
        {error && <p className="error small" role="alert">{error}</p>}

        <h3 className="variables-heading">{t("variables.params")}</h3>
        {paramList.length > 0 ? (
          <>
            <ul className="variables-list" data-scope="params">
              {paramList.map(([param, text]) => (
                <li key={param}>
                  <span className="mono variables-name">:{param}</span>
                  <span className="mono variables-value">{text}</span>
                  <button type="button" className="ghost" aria-label={t("variables.forgetNamed", { name: param })} title={t("variables.forget")} onClick={() => {
                    const next = { ...params };
                    delete next[param];
                    updateTab(tabId, { params: next });
                  }}>
                    <Icon name="x" />
                  </button>
                </li>
              ))}
            </ul>
          </>
        ) : <p className="muted small">{t("variables.noneParams")}</p>}

        <div className="modal-actions">
          <button type="button" onClick={onClose}>{t("variables.close")}</button>
        </div>
      </form>
    </div>
  );
}
