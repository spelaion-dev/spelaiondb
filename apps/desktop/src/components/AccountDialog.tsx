// Accounts of the server (ADR-048): create a role, user or login, change its password, lock or
// unlock it, drop it. The core writes the SQL with the password masked, the dialog shows it,
// and Apply runs exactly that script. The password is sent once, on Apply, and never shown.

import { useEffect, useId, useMemo, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands } from "../api";
import type {
  AlterationPreviewItem,
  PrincipalActionsItem,
  PrincipalChangeItem,
  PrincipalKindItem,
  PrincipalTargetItem,
} from "../bindings";
import { connectTabFromExplorer } from "../connect";
import { useApp, useDialogs } from "../store";

const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** What the dialog does: create an account of a kind, or act on one the explorer lists. */
export type AccountAction =
  | { action: "create"; kind: PrincipalKindItem }
  | { action: "password" | "lock" | "drop"; target: PrincipalTargetItem };

const actionsBySession = new Map<string, PrincipalActionsItem>();

/** Which accounts the session's engine can create, give a password, lock or drop (the core decides). */
export function usePrincipalActions(session: string, enabled: boolean): PrincipalActionsItem | undefined {
  const [actions, setActions] = useState(() => actionsBySession.get(session));
  useEffect(() => {
    if (actions || !enabled) return;
    let current = true;
    call(commands.principalActions(session))
      .then((answer) => {
        actionsBySession.set(session, answer);
        if (current) setActions(answer);
      })
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [session, enabled, actions]);
  return actions;
}

export function AccountDialog({ session, connectionId, request, logins, onClose, onDone }: {
  session: string;
  connectionId: string;
  request: AccountAction;
  /** SQL Server logins the explorer lists, offered for a new user. */
  logins: string[];
  onClose(): void;
  /** After it ran: the explorer reads the accounts again. */
  onDone(): void;
}) {
  const { t } = useTranslation("shell");
  const connection = useApp((s) => s.connections.find((c) => c.id === connectionId));
  const engine = connection?.engine === "mariadb" ? "mysql" : connection?.engine ?? "";
  const kind = request.action === "create" ? request.kind : request.target.kind;
  const [name, setName] = useState("");
  const [host, setHost] = useState("");
  const [password, setPassword] = useState("");
  const [repeat, setRepeat] = useState("");
  const [canLogin, setCanLogin] = useState(true);
  const [superuser, setSuperuser] = useState(false);
  const [createDb, setCreateDb] = useState(false);
  const [createRole, setCreateRole] = useState(false);
  const [defaultDatabase, setDefaultDatabase] = useState("");
  const [checkPolicy, setCheckPolicy] = useState(true);
  const [login, setLogin] = useState("");
  const [defaultSchema, setDefaultSchema] = useState("");
  /** Lock: the state read from the server (null when unknown), and the one chosen. */
  const [locked, setLocked] = useState<boolean | null>(null);
  const [lock, setLock] = useState(true);
  const [preview, setPreview] = useState<{ key: string; value: AlterationPreviewItem } | null>(null);
  const [previewError, setPreviewError] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const ids = useId();
  const previousFocus = useRef(document.activeElement instanceof HTMLElement ? document.activeElement : null);
  useEffect(() => () => { if (previousFocus.current?.isConnected) previousFocus.current.focus(); }, []);

  useEffect(() => {
    if (request.action !== "lock") return;
    let current = true;
    call(commands.principalLocked(session, request.target))
      .then((state) => {
        if (!current) return;
        setLocked(state);
        if (state !== null) setLock(!state);
      })
      .catch(() => undefined);
    return () => { current = false; };
  }, [session, request]);

  /** What needs a password: a login always, a PostgreSQL role or MySQL user when one is typed. */
  const asksPassword =
    request.action === "password" ||
    (request.action === "create" &&
      ((engine === "postgres") || (engine === "mysql" && kind === "user") || (engine === "sqlserver" && kind === "login")));
  /** A password change, and a SQL Server login, cannot go without one. */
  const required = request.action === "password" || (request.action === "create" && engine === "sqlserver" && kind === "login");
  const withPassword = asksPassword && (required || password !== "");
  const mismatch = asksPassword && repeat !== "" && password !== repeat;
  const passwordReady = !asksPassword || (password === repeat && (password !== "" || !required));

  const change: PrincipalChangeItem = useMemo(() => {
    switch (request.action) {
      case "create":
        return {
          action: "create",
          account: {
            kind, name, host, withPassword, canLogin, superuser, createDb, createRole,
            defaultDatabase, checkPolicy, login, defaultSchema,
          },
        };
      case "password":
        return { action: "setPassword", target: request.target };
      case "lock":
        return { action: lock ? "lock" : "unlock", target: request.target };
      case "drop":
        return { action: "drop", target: request.target };
    }
  }, [request, kind, name, host, withPassword, canLogin, superuser, createDb, createRole, defaultDatabase, checkPolicy, login, defaultSchema, lock]);
  const key = JSON.stringify(change);

  useEffect(() => {
    let current = true;
    const timer = window.setTimeout(() => {
      call(commands.previewPrincipalChange(session, JSON.parse(key) as PrincipalChangeItem))
        .then((value) => { if (current) { setPreview({ key, value }); setPreviewError(null); } })
        .catch((e: unknown) => { if (current) { setPreview(null); setPreviewError(message(e)); } });
    }, 150);
    return () => { current = false; window.clearTimeout(timer); };
  }, [session, key]);

  const ready = preview !== null && preview.key === key && passwordReady;
  const readOnly = connection?.readOnly ?? false;
  const production = connection?.kind?.safeguards.confirmDataChanges ?? false;
  const drop = request.action === "drop";

  async function apply() {
    if (!ready || !preview) return;
    const shown = preview.value;
    let confirmed = drop;
    if (!drop && shown.risks.length > 0) {
      const statements = shown.risks.map((r) => ({ sql: shown.statements[r.statement] ?? shown.sql, risk: r.risk }));
      if (!(await useDialogs.getState().askRisks(connection?.name ?? "", statements))) return;
      confirmed = true;
    }
    setBusy(true);
    setError(null);
    try {
      await call(commands.applyPrincipalChange(session, change, withPassword ? password : null, shown.sql, confirmed));
      onDone();
    } catch (e) {
      setError(t("accounts.failed", { detail: message(e) }));
    } finally {
      setBusy(false);
    }
  }

  function openInEditor() {
    if (!ready || !preview || withPassword) return;
    const tab = useApp.getState().addTab(connectionId, preview.value.sql);
    connectTabFromExplorer(tab, session);
    onClose();
  }

  const target = request.action === "create" ? "" : request.target.name;
  const title =
    request.action === "create" ? t(`accounts.newTitle.${kind}`)
    : request.action === "password" ? t("accounts.passwordTitle", { name: target })
    : request.action === "lock" ? t("accounts.lockTitle", { name: target })
    : t("accounts.dropTitle", { name: target });
  const actionLabel =
    request.action === "create" ? t("accounts.create")
    : request.action === "password" ? t("accounts.setPassword")
    : request.action === "lock" ? (lock ? t("accounts.lock") : t("accounts.unlock"))
    : t("objectChange.drop");

  return (
    <div className="modal-backdrop">
      <form className="modal object-change-dialog account-dialog" role="dialog" aria-modal="true" aria-label={title}
        autoComplete="off"
        onSubmit={(e) => { e.preventDefault(); if (ready && !readOnly && !busy) void apply(); }}
        onKeyDown={(e) => { if (e.key === "Escape") { e.stopPropagation(); if (!busy) onClose(); } }}>
        <h2>{title}</h2>
        {request.action === "create" && (
          <>
            <label className="field">
              <span>{t("accounts.name")}</span>
              <input type="text" className="mono" value={name} autoFocus spellCheck={false} onChange={(e) => setName(e.target.value)} />
            </label>
            {engine === "mysql" && kind === "user" && (
              <div className="field">
                <label htmlFor={`${ids}-host`}>{t("accounts.host")}</label>
                <input id={`${ids}-host`} type="text" className="mono" value={host} placeholder="%" spellCheck={false} aria-describedby={`${ids}-host-hint`} onChange={(e) => setHost(e.target.value)} />
                <span id={`${ids}-host-hint`} className="muted small">{t("accounts.hostHint")}</span>
              </div>
            )}
            {engine === "sqlserver" && kind === "user" && (
              <label className="field">
                <span>{t("accounts.login")}</span>
                <input type="text" className="mono" value={login} list="account-logins" spellCheck={false} placeholder={t("accounts.withoutLogin")} onChange={(e) => setLogin(e.target.value)} />
                <datalist id="account-logins">{logins.map((l) => <option key={l} value={l} />)}</datalist>
              </label>
            )}
            {engine === "sqlserver" && kind === "user" && (
              <label className="field">
                <span>{t("accounts.defaultSchema")}</span>
                <input type="text" className="mono" value={defaultSchema} placeholder="dbo" spellCheck={false} onChange={(e) => setDefaultSchema(e.target.value)} />
              </label>
            )}
            {engine === "sqlserver" && kind === "login" && (
              <label className="field">
                <span>{t("accounts.defaultDatabase")}</span>
                <input type="text" className="mono" value={defaultDatabase} placeholder="master" spellCheck={false} onChange={(e) => setDefaultDatabase(e.target.value)} />
              </label>
            )}
          </>
        )}
        {asksPassword && (
          <>
            <div className="field">
              <label htmlFor={`${ids}-password`}>{request.action === "password" ? t("accounts.newPassword") : t("accounts.password")}</label>
              <input id={`${ids}-password`} type="password" value={password} autoComplete="new-password" autoFocus={request.action === "password"}
                aria-describedby={required ? undefined : `${ids}-password-hint`} onChange={(e) => setPassword(e.target.value)} />
              {!required && <span id={`${ids}-password-hint`} className="muted small">{t("accounts.passwordOptional")}</span>}
            </div>
            <div className="field">
              <label htmlFor={`${ids}-repeat`}>{t("accounts.repeatPassword")}</label>
              <input id={`${ids}-repeat`} type="password" value={repeat} autoComplete="new-password" aria-invalid={mismatch}
                aria-describedby={mismatch ? `${ids}-mismatch` : undefined} onChange={(e) => setRepeat(e.target.value)} />
              {mismatch && <span id={`${ids}-mismatch`} className="error small" role="alert">{t("accounts.mismatch")}</span>}
            </div>
          </>
        )}
        {request.action === "create" && engine === "postgres" && (
          <fieldset className="account-options">
            <legend>{t("accounts.options")}</legend>
            <label className="check"><input type="checkbox" checked={canLogin} onChange={(e) => setCanLogin(e.target.checked)} /> {t("accounts.canLogin")}</label>
            <label className="check"><input type="checkbox" checked={createDb} onChange={(e) => setCreateDb(e.target.checked)} /> {t("accounts.createDb")}</label>
            <label className="check"><input type="checkbox" checked={createRole} onChange={(e) => setCreateRole(e.target.checked)} /> {t("accounts.createRole")}</label>
            <label className="check"><input type="checkbox" checked={superuser} onChange={(e) => setSuperuser(e.target.checked)} /> {t("accounts.superuser")}</label>
          </fieldset>
        )}
        {request.action === "create" && engine === "sqlserver" && kind === "login" && (
          <label className="check"><input type="checkbox" checked={checkPolicy} onChange={(e) => setCheckPolicy(e.target.checked)} /> {t("accounts.checkPolicy")}</label>
        )}
        {request.action === "lock" && (
          <>
            <p className="muted small">
              {locked === null ? t("accounts.stateUnknown") : locked ? t("accounts.isLocked") : t("accounts.isUnlocked")}
            </p>
            <div className="form-row" role="radiogroup" aria-label={t("accounts.lockChoice")}>
              <label className="check"><input type="radio" name="account-lock" checked={lock} onChange={() => setLock(true)} /> {t("accounts.lockOption")}</label>
              <label className="check"><input type="radio" name="account-lock" checked={!lock} onChange={() => setLock(false)} /> {t("accounts.unlockOption")}</label>
            </div>
          </>
        )}
        <span className="muted small">{t("objectChange.sql")}</span>
        <pre className="object-change-sql mono" aria-label={t("objectChange.sql")} aria-live="polite">
          {preview?.key === key ? preview.value.sql : previewError ?? "…"}
        </pre>
        {withPassword && <p className="muted small">{t("accounts.masked")}</p>}
        {production && <p className="warn small">{t("objectChange.production")}</p>}
        {readOnly && <p className="warn small">{t("accounts.readOnly")}</p>}
        {error && <p className="error small" role="alert">{error}</p>}
        <div className="modal-actions">
          {!withPassword && (
            <button type="button" onClick={openInEditor} disabled={!ready || busy}>{t("objectChange.openInEditor")}</button>
          )}
          <span className="spacer" />
          <button type="button" onClick={onClose} disabled={busy} autoFocus={drop}>{t("cancel")}</button>
          <button type="submit" className={drop ? "danger" : "primary"} disabled={!ready || readOnly || busy}>
            {busy ? t("objectChange.running") : actionLabel}
          </button>
        </div>
      </form>
    </div>
  );
}
