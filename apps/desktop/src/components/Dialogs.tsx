// Modal dialogs: password, repository trust, risky statements and SSH tunnel questions.

import { useEffect, useRef, useState } from "react";
import { Trans, useTranslation } from "react-i18next";
import { call, commands } from "../api";
import { useDialogs } from "../store";

export function Dialogs() {
  const password = useDialogs((s) => s.password);
  const trust = useDialogs((s) => s.trust);
  const risk = useDialogs((s) => s.risk);
  const interaction = useDialogs((s) => s.interactions[0]);
  if (interaction) return <InteractionDialog key={interaction.id} />;
  if (trust) return <TrustDialog />;
  if (risk) return <RiskDialog />;
  if (password) return <PasswordDialog key={password.connectionName + (password.error ?? "")} />;
  return null;
}

function Modal({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="modal-backdrop">
      <div className="modal" role="dialog" aria-modal="true" aria-label={title}>
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
    <Modal title={t("passwordFor", { connection: request.connectionName })}>
      {request.error && <p className="error">{request.error}</p>}
      <input
        ref={input}
        type="password"
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") submit();
          if (e.key === "Escape") request.resolve(null);
        }}
        autoComplete="off"
        aria-label={t("password")}
      />
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
    <Modal title={t("trustRepository")}>
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

/** Long statements are cut in the list: the reason is what matters. */
const SHOWN_SQL = 400;

function RiskDialog() {
  const { t } = useTranslation("shell");
  const request = useDialogs((s) => s.risk);
  const cancel = useRef<HTMLButtonElement>(null);
  // Cancel has the focus: Enter by habit must not run a DROP.
  useEffect(() => cancel.current?.focus(), []);
  if (!request) return null;
  return (
    <Modal title={t("risk.title")}>
      <p>{t("risk.explanation", { connection: request.connection })}</p>
      <ul className="risk-list">
        {request.statements.map((s, i) => (
          <li key={i}>
            <strong>{t(`risk.${s.risk}`)}</strong>
            <pre className="mono small">{s.sql.length > SHOWN_SQL ? `${s.sql.slice(0, SHOWN_SQL)}…` : s.sql}</pre>
          </li>
        ))}
      </ul>
      <div className="modal-actions" onKeyDown={(e) => e.key === "Escape" && request.resolve(false)}>
        <button ref={cancel} onClick={() => request.resolve(false)}>
          {t("risk.cancel")}
        </button>
        <button className="danger" onClick={() => request.resolve(true)}>
          {t("risk.run")}
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
      <Modal title={t("unknownSshServer")}>
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
    case "challenge":
      title = label.instructions || t("verificationFor", { host: label.host });
      prompt = r.prompt;
      break;
  }
  return (
    <Modal title={title}>
      <p className="mono small">{prompt}</p>
      <input
        autoFocus
        type={r.echo ? "text" : "password"}
        value={value}
        onChange={(e) => setValue(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") answer(true);
          if (e.key === "Escape") answer(false);
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
