// Modal dialogs: password, repository trust, changes to trusted repository items, query
// parameters, risky statements, a script paused after an error and SSH tunnel questions.

import { useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { call, commands } from "../api";
import { useDialogs } from "../store";

export function Dialogs() {
  const password = useDialogs((s) => s.password);
  const trust = useDialogs((s) => s.trust);
  const change = useDialogs((s) => s.change);
  const variables = useDialogs((s) => s.variables);
  const risk = useDialogs((s) => s.risk);
  const scriptError = useDialogs((s) => s.scriptError);
  const openTransactions = useDialogs((s) => s.openTransactions);
  const params = useDialogs((s) => s.params);
  const text = useDialogs((s) => s.text);
  const interaction = useDialogs((s) => s.interactions[0]);
  if (interaction) return <InteractionDialog key={interaction.id} />;
  if (trust) return <TrustDialog />;
  if (change) return <ChangeDialog key={change.item} />;
  if (variables) return <VariablesDialog key={variables.names.join(",")} />;
  if (params) return <ParamsDialog />;
  if (text) return <TextDialog />;
  if (risk) return <RiskDialog />;
  // Closing a tab whose run waits for this answer asks first; closing it stops that run.
  if (openTransactions) return <OpenTransactionsDialog />;
  if (scriptError) return <ScriptErrorDialog key={`${scriptError.tab}-${scriptError.statement}`} />;
  if (password) return <PasswordDialog key={password.connectionName + (password.error ?? "")} />;
  return null;
}

function Modal({ title, children, onCancel }: { title: string; children: React.ReactNode; onCancel(): void }) {
  return (
    <div className="modal-backdrop">
      <div className="modal" role="dialog" aria-modal="true" aria-label={title} onKeyDown={(e) => {
        if (e.key === "Escape") { e.preventDefault(); e.stopPropagation(); onCancel(); }
      }}>
        <h2>{title}</h2>
        {children}
      </div>
    </div>
  );
}

function PasswordDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.password);
  const [value, setValue] = useState("");
  const [remember, setRemember] = useState(false);
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => input.current?.focus(), []);
  if (!request) return null;
  const submit = () => request.resolve({ password: value, remember });
  return (
    <Modal title={t("passwordFor", { connection: request.connectionName })} onCancel={() => request.resolve(null)}>
      {request.error && <p className="error">{request.error}</p>}
      <label className="modal-field">
        <span>{t("password")}</span>
        <input
          ref={input}
          type="password"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
          autoComplete="off"
        />
      </label>
      {request.canRemember && (
        <label className="check">
          <input type="checkbox" checked={remember} onChange={(e) => setRemember(e.target.checked)} />
          {t("rememberPassword")}
        </label>
      )}
      <div className="modal-actions">
        <button onClick={() => request.resolve(null)}>{t("cancel")}</button>
        <button className="primary" onClick={submit}>
          {t("connect")}
        </button>
      </div>
    </Modal>
  );
}

function TrustDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.trust);
  if (!request) return null;
  return (
    <Modal title={t("trustRepository")} onCancel={() => request.resolve(false)}>
      <p className="mono small">{request.workspace ?? t("thisRepository")}</p>
      <p>
        {t("trustExplanation")}
      </p>
      <p className="muted small">{t("trustAdvice")}</p>
      <div className="modal-actions">
        <button onClick={() => request.resolve(false)}>{t("dontTrust")}</button>
        <button className="primary" onClick={() => request.resolve(true)}>
          {t("trust")}
        </button>
      </div>
    </Modal>
  );
}

/** The personal values a repository connection needs (`${user.…}`, ADR-022), asked once and
 *  saved in the user's `user.toml`, outside the repository. */
function VariablesDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.variables);
  const [values, setValues] = useState<Record<string, string>>({});
  const first = useRef<HTMLInputElement>(null);
  useEffect(() => first.current?.focus(), []);
  if (!request) return null;
  const complete = request.names.every((name) => (values[name] ?? "").trim() !== "");
  const submit = () => {
    if (complete) request.resolve(Object.fromEntries(request.names.map((name) => [name, (values[name] ?? "").trim()])));
  };
  return (
    <Modal title={t("variables.title")} onCancel={() => request.resolve(null)}>
      <p>{t("variables.explanation")}</p>
      <form
        className="variables-form"
        onSubmit={(e) => {
          e.preventDefault();
          submit();
        }}
      >
        {request.names.map((name, i) => (
          <label key={name} className="field">
            <span className="mono">{`\${user.${name}}`}</span>
            <input
              ref={i === 0 ? first : undefined}
              type="text"
              aria-label={name}
              autoComplete="off"
              value={values[name] ?? ""}
              onChange={(e) => setValues((v) => ({ ...v, [name]: e.target.value }))}
            />
          </label>
        ))}
        <p className="muted small">{t("variables.advice")}</p>
        <div className="modal-actions">
          <button type="button" onClick={() => request.resolve(null)}>
            {t("cancel")}
          </button>
          <button type="submit" className="primary" disabled={!complete}>
            {t("variables.save")}
          </button>
        </div>
      </form>
    </Modal>
  );
}

/** What changed in a repository connection or tunnel since the user trusted it: nothing of
 *  it is used (no secret read, no command run) until they accept (ADR-022). */
function ChangeDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.change);
  if (!request) return null;
  const [kind, name] = request.item.includes(":") ? request.item.split(/:(.*)/s) : [request.item, ""];
  const what =
    kind === "tunnel"
      ? t("change.tunnel", { name })
      : kind === "known_hosts"
        ? t("change.knownHosts")
        : t("change.connection", { name });
  return (
    <Modal title={request.isNew ? t("change.newTitle") : t("change.title")} onCancel={() => request.resolve(false)}>
      <p>{request.isNew ? t("change.newExplanation", { what }) : t("change.explanation", { what })}</p>
      <div className="change-table-wrap">
        <table className="change-table">
          <thead>
            <tr>
              <th>{t("change.field")}</th>
              {!request.isNew && <th>{t("change.before")}</th>}
              <th>{request.isNew ? t("change.value") : t("change.after")}</th>
            </tr>
          </thead>
          <tbody>
            {request.fields.map((f) => (
              <tr key={f.field}>
                <td className="mono">{f.field}</td>
                {!request.isNew && <td className="mono">{f.before ?? t("change.none")}</td>}
                <td className="mono">{f.after ?? t("change.none")}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
      <p className="muted small">{t("change.advice")}</p>
      <div className="modal-actions">
        <button onClick={() => request.resolve(false)}>{t("change.reject")}</button>
        <button className="primary" onClick={() => request.resolve(true)}>
          {t("change.accept")}
        </button>
      </div>
    </Modal>
  );
}

function TextDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.text);
  const [value, setValue] = useState(request?.initial ?? "");
  if (!request) return null;
  const submit = () => value.trim() && request.resolve(value.trim());
  return (
    <Modal title={request.title} onCancel={() => request.resolve(null)}>
      <label className="field">
        <span>{request.label}</span>
        <input
          autoFocus
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onFocus={(e) => e.currentTarget.select()}
          onKeyDown={(e) => {
            if (e.key === "Enter") submit();
          }}
          maxLength={200}
        />
      </label>
      <div className="modal-actions">
        <button onClick={() => request.resolve(null)}>{t("params.cancel")}</button>
        <button className="primary" disabled={!value.trim()} onClick={submit}>
          {request.confirm}
        </button>
      </div>
    </Modal>
  );
}

function ParamsDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.params);
  const [values, setValues] = useState<Record<string, string>>(() =>
    Object.fromEntries((request?.names ?? []).map((n) => [n, request?.previous[n] ?? ""])),
  );
  if (!request) return null;
  const submit = () => request.resolve(values);
  return (
    <Modal title={t("params.title")} onCancel={() => request.resolve(null)}>
      <p className="muted small">{t("params.explanation")}</p>
      <div
        className="params-list"
        onKeyDown={(e) => {
          if (e.key === "Enter") submit();
        }}
      >
        {request.names.map((name, i) => (
          <label key={name} className="field">
            <span className="mono">:{name}</span>
            <input
              autoFocus={i === 0}
              value={values[name] ?? ""}
              onChange={(e) => setValues((v) => ({ ...v, [name]: e.target.value }))}
              spellCheck={false}
              autoComplete="off"
            />
          </label>
        ))}
      </div>
      <div className="modal-actions">
        <button onClick={() => request.resolve(null)}>{t("params.cancel")}</button>
        <button className="primary" onClick={submit}>
          {t("params.run")}
        </button>
      </div>
    </Modal>
  );
}

/** Long statements are cut in the list: the reason is what matters. */
const SHOWN_SQL = 400;
/** A connection that asks before every run lists at most this many statements. */
const SHOWN_STATEMENTS = 20;

function RiskDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.risk);
  const cancel = useRef<HTMLButtonElement>(null);
  // Cancel has the focus: Enter by habit must not run a DROP.
  useEffect(() => cancel.current?.focus(), []);
  if (!request) return null;
  // Only because the connection's type asks before every run (ADR-029): nothing risky as such.
  const onlyExecution = request.statements.every((s) => s.risk === "execution");
  const hidden = request.statements.length - SHOWN_STATEMENTS;
  return (
    <Modal title={t("risk.title")} onCancel={() => request.resolve(false)}>
      <p>{t(onlyExecution ? "risk.executionExplanation" : "risk.explanation", { connection: request.connection })}</p>
      <ul className="risk-list">
        {request.statements.slice(0, SHOWN_STATEMENTS).map((s, i) => (
          <li key={i}>
            <strong>{t(`risk.${s.risk}`)}</strong>
            <pre className="mono small">{s.sql.length > SHOWN_SQL ? `${s.sql.slice(0, SHOWN_SQL)}…` : s.sql}</pre>
          </li>
        ))}
        {hidden > 0 && <li className="muted small">{t("risk.more", { count: hidden })}</li>}
      </ul>
      <div className="modal-actions">
        <button ref={cancel} onClick={() => request.resolve(false)}>
          {t("risk.cancel")}
        </button>
        <button className={onlyExecution ? undefined : "danger"} onClick={() => request.resolve(true)}>
          {t(onlyExecution ? "risk.runConfirmed" : "risk.run")}
        </button>
      </div>
    </Modal>
  );
}

/** A run paused after an error (ADR-035). Stop has the focus and Escape stops: going on is
 *  always a choice. The core decides what each answer runs. */
function ScriptErrorDialog() {
  const { t } = useTranslation("editor");
  const request = useDialogs((s) => s.scriptError);
  const stop = useRef<HTMLButtonElement>(null);
  useEffect(() => stop.current?.focus(), []);
  if (!request) return null;
  return (
    <Modal title={t("scriptError.title", { index: request.statement })} onCancel={() => request.resolve("stop")}>
      <pre className="error-box">{request.message}</pre>
      <p>{t("scriptError.remaining", { count: request.remaining })}</p>
      <p className="muted small">{t("scriptError.hint")}</p>
      <div className="modal-actions">
        <button ref={stop} onClick={() => request.resolve("stop")}>
          {t("scriptError.stop")}
        </button>
        <button onClick={() => request.resolve("skip")}>{t("scriptError.skip")}</button>
        <button onClick={() => request.resolve("skipAll")}>{t("scriptError.skipAll")}</button>
      </div>
    </Modal>
  );
}

/** Closing, disconnecting or quitting with a transaction open (ADR-031): the server rolls
 *  back what was not committed, so the user decides. Cancel has the focus. */
function OpenTransactionsDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.openTransactions);
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => cancel.current?.focus(), []);
  if (!request) return null;
  const transactions = request.tabs.length > 0;
  // Only running queries: closing cancels them, nothing is rolled back.
  const proceed = transactions ? t(`openTransactions.proceed.${request.action}`) : t("openTransactions.proceedRunning");
  return (
    <Modal title={transactions ? t(`openTransactions.title.${request.action}`) : t("openTransactions.titleRunning")} onCancel={() => request.resolve(false)}>
      {transactions && (
        <>
          <p>{t("openTransactions.explanation", { count: request.tabs.length })}</p>
          <ul className="risk-list open-transaction-list">
            {request.tabs.map((tab) => <li key={tab}><strong>{tab}</strong></li>)}
          </ul>
        </>
      )}
      {request.running.length > 0 && (
        <>
          <p>{t("openTransactions.running", { count: request.running.length })}</p>
          <ul className="risk-list running-list">
            {request.running.map((tab) => <li key={tab}><strong>{tab}</strong></li>)}
          </ul>
        </>
      )}
      {transactions && <p className="muted small">{t("openTransactions.hint")}</p>}
      <div className="modal-actions">
        <button ref={cancel} onClick={() => request.resolve(false)}>
          {t("openTransactions.cancel")}
        </button>
        <button className="danger" onClick={() => request.resolve(true)}>
          {proceed}
        </button>
      </div>
    </Modal>
  );
}

function InteractionDialog() {
  const { t } = useTranslation("shell");
  const item = useDialogs((s) => s.interactions[0]);
  const pop = useDialogs((s) => s.popInteraction);
  const [value, setValue] = useState("");
  if (!item) return null;
  const answer = (accept: boolean) => {
    void call(commands.answerInteraction(item.id, accept, accept && item.request.kind === "secret" ? value : null));
    pop(item.id);
  };
  const r = item.request;
  if (r.kind === "unknownHost") {
    return (
      <Modal title={t("unknownSshServer")} onCancel={() => answer(false)}>
        <p>
          <Trans t={t} i18nKey="unknownHostPrompt" values={{ host: r.host, port: r.port, hop: r.hop }} components={{ strong: <strong /> }} />
        </p>
        <p className="mono fingerprint">
          {r.algorithm} {r.fingerprint}
        </p>
        <div className="modal-actions">
          <button onClick={() => answer(false)}>{t("cancel")}</button>
          <button className="primary" onClick={() => answer(true)}>
            {t("trustAndRemember")}
          </button>
        </div>
      </Modal>
    );
  }
  const label = r.label;
  let title: string;
  let prompt: string;
  switch (label.kind) {
    case "keyPassphrase":
      title = t("sshKeyPassphrase");
      prompt = label.retry ? t("wrongPassphrase", { path: label.path }) : label.path;
      break;
    case "password":
      title = t("sshPassword");
      prompt = `${label.user}@${label.host}`;
      break;
    case "proxyPassword":
      title = t("proxyPassword");
      prompt = `${label.user}@${label.host}`;
      break;
    case "challenge":
      title = label.instructions || t("verificationFor", { host: label.host });
      prompt = r.prompt;
      break;
  }
  return (
    <Modal title={title} onCancel={() => answer(false)}>
      <p className="mono small">{prompt}</p>
      <input
        autoFocus
        type={r.echo ? "text" : "password"}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") answer(true);
        }}
        autoComplete="off"
        aria-label={prompt}
      />
      <div className="modal-actions">
        <button onClick={() => answer(false)}>{t("cancel")}</button>
        <button className="primary" onClick={() => answer(true)}>
          {t("ok")}
        </button>
      </div>
    </Modal>
  );
}
