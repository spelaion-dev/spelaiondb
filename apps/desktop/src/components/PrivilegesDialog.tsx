// An account's privileges (ADR-048): what it has, read from the catalog by the core, and a form
// that grants or revokes privileges globally, on a database, a schema, one object, some columns
// of a table, PostgreSQL's default privileges of objects created later, or a role's membership.
// The core writes the GRANT or REVOKE, the dialog shows it, and it runs as shown.

import { useCallback, useEffect, useId, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { TFunction } from "i18next";
import { call, commands } from "../api";
import type {
  AlterationPreviewItem,
  GrantedPrivilegeItem,
  PrincipalTargetItem,
  PrivilegeChangeItem,
  PrivilegeScopeInfoItem,
  PrivilegeScopeItem,
  PrivilegeTargetItem,
} from "../bindings";
import { useApp, useDialogs } from "../store";
import { Icon } from "./Icons";

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** PostgreSQL's default privileges of objects created later. */
function isDefault(scope: PrivilegeScopeItem | null): boolean {
  return scope !== null && scope.startsWith("default_");
}

/** Column names typed in the form: separated by commas. */
function splitColumns(text: string): string[] {
  return text.split(",").map((c) => c.trim()).filter((c) => c !== "");
}

/** Where a granted privilege applies, as one line: `shop.orders`, `s.t (a, b)`, a role's name,
 *  "in s, created by owner", the server. */
function placeOf(target: PrivilegeTargetItem, t: TFunction<"shell">): string {
  if (isDefault(target.scope)) {
    return target.schema === ""
      ? t("privileges.everySchemaBy", { role: target.name })
      : t("privileges.inSchemaBy", { schema: target.schema, role: target.name });
  }
  const place = [target.database, target.schema, target.name].filter((part) => part !== "").join(".");
  return target.columns.length > 0 ? `${place} (${target.columns.join(", ")})` : place;
}

export function PrivilegesDialog({ session, connectionId, target, onClose }: {
  session: string;
  connectionId: string;
  target: PrincipalTargetItem;
  onClose(): void;
}) {
  const { t } = useTranslation("shell");
  const connection = useApp((s) => s.connections.find((c) => c.id === connectionId));
  const ids = useId();
  const [scopes, setScopes] = useState<PrivilegeScopeInfoItem[] | null>(null);
  const [granted, setGranted] = useState<GrantedPrivilegeItem[] | null>(null);
  const [grantedError, setGrantedError] = useState<string | null>(null);
  const [revoke, setRevoke] = useState(false);
  const [scope, setScope] = useState<PrivilegeScopeItem | null>(null);
  const [database, setDatabase] = useState("");
  const [schema, setSchema] = useState("");
  const [name, setName] = useState("");
  const [columns, setColumns] = useState("");
  const [chosen, setChosen] = useState<string[]>([]);
  const [grantOption, setGrantOption] = useState(false);
  const [preview, setPreview] = useState<{ key: string; value: AlterationPreviewItem } | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [done, setDone] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const previousFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  useEffect(() => () => { if (previousFocus.current?.isConnected) previousFocus.current.focus(); }, []);

  useEffect(() => {
    let current = true;
    call(commands.privilegeScopes(session, target.kind))
      .then((answer) => {
        if (!current) return;
        setScopes(answer);
        setScope((s) => s ?? answer[0]?.scope ?? null);
      })
      .catch((e: unknown) => { if (current) setGrantedError(message(e)); });
    return () => { current = false; };
  }, [session, target.kind]);

  const readGranted = useCallback(() => {
    setGrantedError(null);
    call(commands.principalPrivileges(session, target))
      .then(setGranted)
      .catch((e: unknown) => { setGranted([]); setGrantedError(message(e)); });
  }, [session, target]);
  useEffect(readGranted, [readGranted]);

  const info = scopes?.find((s) => s.scope === scope) ?? null;
  const isRole = scope === "role";
  const defaults = isDefault(scope);
  const change: PrivilegeChangeItem | null = useMemo(() => scope === null ? null : ({
    grantee: target,
    target: {
      scope,
      database: info?.needsDatabase ? database : "",
      schema: info?.needsSchema ? schema : "",
      name: info?.needsName ? name : "",
      columns: info?.needsColumns ? splitColumns(columns) : [],
    },
    privileges: isRole ? [] : chosen,
    revoke,
    grantOption,
  }), [target, scope, info, database, schema, name, columns, chosen, revoke, grantOption, isRole]);
  const key = JSON.stringify(change);

  useEffect(() => {
    if (!change) return;
    let current = true;
    const timer = window.setTimeout(() => {
      call(commands.previewPrivilegeChange(session, JSON.parse(key) as PrivilegeChangeItem))
        .then((value) => { if (current) { setPreview({ key, value }); setPreviewError(null); } })
        .catch((e: unknown) => { if (current) { setPreview(null); setPreviewError(message(e)); } });
    }, 150);
    return () => { current = false; window.clearTimeout(timer); };
  }, [session, key]);

  const ready = change !== null && preview !== null && preview.key === key;
  const readOnly = connection?.readOnly ?? false;
  const production = connection?.kind?.safeguards.confirmDataChanges ?? false;

  function pickScope(next: PrivilegeScopeItem) {
    setScope(next);
    setChosen([]);
    setGrantOption(false);
  }

  /** "Revoke…" on a row of what it has: the form, set to take that privilege away. */
  function revokeRow(row: GrantedPrivilegeItem) {
    setRevoke(true);
    setScope(row.target.scope);
    setDatabase(row.target.database);
    setSchema(row.target.schema);
    setName(row.target.name);
    setColumns(row.target.columns.join(", "));
    setChosen(row.target.scope === "role" ? [] : [row.privilege.toUpperCase()]);
    setGrantOption(false);
    setDone(null);
  }

  async function apply() {
    if (!ready || !preview || !change) return;
    const shown = preview.value;
    let confirmed = false;
    if (shown.risks.length > 0) {
      const statements = shown.risks.map((r) => ({ sql: shown.statements[r.statement] ?? shown.sql, risk: r.risk }));
      if (!(await useDialogs.getState().askRisks(connection?.name ?? "", statements))) return;
      confirmed = true;
    }
    setBusy(true);
    setError(null);
    setDone(null);
    try {
      await call(commands.applyPrivilegeChange(session, change, shown.sql, confirmed));
      setDone(revoke ? t("privileges.revoked") : t("privileges.granted"));
      setChosen([]);
      readGranted();
    } catch (e) {
      setError(t("privileges.failed", { detail: message(e) }));
    } finally {
      setBusy(false);
    }
  }

  const title = t("privileges.title", { name: target.name });
  return (
    <div className="modal-backdrop">
      <form className="modal structure-dialog privileges-dialog" role="dialog" aria-modal="true" aria-label={title}
        onSubmit={(e) => { e.preventDefault(); if (ready && !readOnly && !busy) void apply(); }}
        onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); if (!busy) onClose(); } }}>
        <h2>{title}</h2>
        <section className="privileges-granted" aria-labelledby={`${ids}-has`}>
          <h3 id={`${ids}-has`}>{t("privileges.has")}</h3>
          {granted === null ? (
            <p className="muted small">{t("privileges.loading")}</p>
          ) : grantedError ? (
            <p className="error small" role="alert">{t("privileges.readFailed", { detail: grantedError })}</p>
          ) : granted.length === 0 ? (
            <p className="muted small">{t("privileges.none")}</p>
          ) : (
            <div className="structure-scroll">
              <table>
                <thead>
                  <tr>
                    <th scope="col">{t("privileges.scope")}</th>
                    <th scope="col">{t("privileges.on")}</th>
                    <th scope="col">{t("privileges.privilege")}</th>
                    <th scope="col" className="structure-icon"><span className="sr-only">{t("privileges.actions")}</span></th>
                  </tr>
                </thead>
                <tbody>
                  {granted.map((row, index) => (
                    <tr key={index}>
                      <td>{t(`privileges.scopes.${row.target.scope}`)}</td>
                      <td className="mono">{placeOf(row.target, t) || "—"}</td>
                      <td className="mono">
                        {row.privilege}
                        {row.grantable && <span className="tag" title={t("privileges.grantableHint")}>{t("privileges.grantable")}</span>}
                        {row.denied && <span className="tag" title={t("privileges.deniedHint")}>{t("privileges.denied")}</span>}
                      </td>
                      <td>
                        {!row.denied && (scopes?.some((s) => s.scope === row.target.scope) ?? false) && (
                          <button type="button" className="ghost icon-button" title={t("privileges.revokeRow")} aria-label={t("privileges.revokeRowOf", { privilege: row.privilege, on: placeOf(row.target, t) })}
                            onClick={() => revokeRow(row)}><Icon name="x" /></button>
                        )}
                      </td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </section>

        <fieldset className="privileges-form">
          <legend>{t("privileges.change")}</legend>
          <div className="form-row" role="radiogroup" aria-label={t("privileges.grantOrRevoke")}>
            <label className="check"><input type="radio" name={`${ids}-mode`} checked={!revoke} onChange={() => setRevoke(false)} /> {t("privileges.grant")}</label>
            <label className="check"><input type="radio" name={`${ids}-mode`} checked={revoke} onChange={() => setRevoke(true)} /> {t("privileges.revoke")}</label>
          </div>
          <div className="field">
            <label htmlFor={`${ids}-scope`}>{t("privileges.scope")}</label>
            <select id={`${ids}-scope`} value={scope ?? ""} onChange={(e) => pickScope(e.target.value as PrivilegeScopeItem)}>
              {(scopes ?? []).map((s) => <option key={s.scope} value={s.scope}>{t(`privileges.scopes.${s.scope}`)}</option>)}
            </select>
          </div>
          {info?.needsDatabase && (
            <div className="field">
              <label htmlFor={`${ids}-database`}>{t("privileges.database")}</label>
              <input id={`${ids}-database`} type="text" className="mono" value={database} spellCheck={false} onChange={(e) => setDatabase(e.target.value)} />
            </div>
          )}
          {info?.needsSchema && (
            <div className="field">
              <label htmlFor={`${ids}-schema`}>{t("privileges.schema")}</label>
              <input id={`${ids}-schema`} type="text" className="mono" value={schema} spellCheck={false}
                aria-describedby={info.schemaOptional ? `${ids}-schema-hint` : undefined} onChange={(e) => setSchema(e.target.value)} />
              {info.schemaOptional && <span id={`${ids}-schema-hint`} className="muted small">{t("privileges.everySchema")}</span>}
            </div>
          )}
          {info?.needsName && (
            <div className="field">
              <label htmlFor={`${ids}-name`}>
                {isRole ? t("privileges.role") : defaults ? t("privileges.createdBy") : info.needsColumns ? t("privileges.table") : t("privileges.object")}
              </label>
              <input id={`${ids}-name`} type="text" className="mono" value={name} spellCheck={false}
                aria-describedby={info.nameOptional ? `${ids}-name-hint` : undefined} onChange={(e) => setName(e.target.value)} />
              {info.nameOptional && <span id={`${ids}-name-hint`} className="muted small">{t("privileges.createdByHint")}</span>}
            </div>
          )}
          {info?.needsColumns && (
            <div className="field">
              <label htmlFor={`${ids}-columns`}>{t("privileges.columns")}</label>
              <input id={`${ids}-columns`} type="text" className="mono" value={columns} spellCheck={false} placeholder="a, b"
                aria-describedby={`${ids}-columns-hint`} onChange={(e) => setColumns(e.target.value)} />
              <span id={`${ids}-columns-hint`} className="muted small">{t("privileges.columnsHint")}</span>
            </div>
          )}
          {defaults && <p className="muted small">{t("privileges.defaultsNote")}</p>}
          {info && !isRole && (
            <div className="privileges-options" role="group" aria-label={t("privileges.privileges")}>
              {info.privileges.map((p) => (
                <label key={p} className="check mono">
                  <input type="checkbox" checked={chosen.includes(p)}
                    onChange={(e) => setChosen((items) => e.target.checked ? [...items, p] : items.filter((item) => item !== p))} /> {p}
                </label>
              ))}
            </div>
          )}
          <label className="check">
            <input type="checkbox" checked={grantOption} onChange={(e) => setGrantOption(e.target.checked)} />
            {isRole
              ? revoke ? t("privileges.adminOptionOnly") : t("privileges.adminOption")
              : revoke ? t("privileges.grantOptionOnly") : t("privileges.grantOption")}
          </label>
        </fieldset>

        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono" aria-label={t("objectChange.sql")} aria-live="polite">
          {ready ? preview.value.sql : previewError ?? "…"}
        </pre>
        {production && <p className="warn small">{t("objectChange.production")}</p>}
        {readOnly && <p className="warn small">{t("accounts.readOnly")}</p>}
        {done && <p className="muted small" role="status">{done}</p>}
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={busy}>{t("privileges.close")}</button>
          <button type="submit" className="primary" disabled={!ready || readOnly || busy}>
            {busy ? t("objectChange.running") : revoke ? t("privileges.revoke") : t("privileges.grant")}
          </button>
        </div>
      </form>
    </div>
  );
}
