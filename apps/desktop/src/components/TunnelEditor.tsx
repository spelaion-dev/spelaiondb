// SSH tunnel editor, opened from the SSH tab of the connection form: the hops in order
// (the last one reaches the database), each with its server and authentication. The core
// validates and writes the file; passwords go to the keychain, never to the file.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import { call, commands, type HopDraft, type PasswordMode, type SshAuthItem, type TunnelDraft, type TunnelItem } from "../api";
import { useApp, useDialogs } from "../store";

export type TunnelEditorMode = { kind: "add"; personal: boolean } | { kind: "edit"; name: string; personal: boolean };

type Method = SshAuthItem | "custom";

interface HopState {
  host: string;
  port: number | null;
  user: string;
  method: Method;
  /** Methods from the file when there are several (shown as "custom" and kept as is). */
  auth: SshAuthItem[];
  identityFile: string;
  /** For `password` / `keyboard-interactive`: keychain when true, asked when false. */
  savePassword: boolean;
  /** Original reference when it is not keychain/prompt (`env:…`, `op://…`): kept as is. */
  fixedMode: PasswordMode | null;
  passwordEnv: string;
  passwordOther: string;
  password: string;
}

const usesPassword = (auth: SshAuthItem[]) => auth.includes("password") || auth.includes("keyboard-interactive");

function emptyHop(): HopState {
  return {
    host: "",
    port: null,
    user: "",
    method: "agent",
    auth: ["agent"],
    identityFile: "",
    savePassword: true,
    fixedMode: null,
    passwordEnv: "",
    passwordOther: "",
    password: "",
  };
}

function fromDraft(hop: HopDraft): HopState {
  const method: Method = hop.auth.length === 1 && hop.auth[0] ? hop.auth[0] : "custom";
  return {
    host: hop.host,
    port: hop.port ?? null,
    user: hop.user,
    method,
    auth: hop.auth,
    identityFile: hop.identityFile,
    savePassword: hop.passwordMode !== "prompt",
    fixedMode: hop.passwordMode === "env" || hop.passwordMode === "other" ? hop.passwordMode : null,
    passwordEnv: hop.passwordEnv,
    passwordOther: hop.passwordOther,
    password: "",
  };
}

function toDraft(hop: HopState): HopDraft {
  const auth: SshAuthItem[] = hop.method === "custom" ? hop.auth : [hop.method];
  const passwordMode: PasswordMode = !usesPassword(auth) ? "none" : (hop.fixedMode ?? (hop.savePassword ? "keychain" : "prompt"));
  return {
    host: hop.host,
    port: hop.port,
    user: hop.user,
    auth,
    identityFile: hop.method === "password" || hop.method === "keyboard-interactive" ? "" : hop.identityFile,
    passwordMode,
    passwordEnv: hop.passwordEnv,
    passwordOther: hop.passwordOther,
  };
}

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function TunnelEditor(props: {
  mode: TunnelEditorMode;
  onClose(): void;
  onSaved(tunnel: TunnelItem): void;
  onDeleted(): void;
}) {
  const { mode } = props;
  const { t } = useTranslation("connections");
  const workspace = useApp((s) => s.workspace);
  const hasWorkspace = Boolean(workspace?.workspace);
  const editing = mode.kind === "edit";
  const [name, setName] = useState(editing ? mode.name : "");
  const [personal, setPersonal] = useState(mode.personal || !hasWorkspace);
  const [hops, setHops] = useState<HopState[]>(editing ? [] : [emptyHop()]);
  const [keepaliveInterval, setKeepaliveInterval] = useState<number | null>(null);
  const [keepaliveCount, setKeepaliveCount] = useState<number | null>(null);
  const [revision, setRevision] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [status, setStatus] = useState<{ kind: "idle" | "testing" | "ok" } | { kind: "error"; message: string }>({ kind: "idle" });
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);

  useEffect(() => {
    if (mode.kind !== "edit") return;
    call(commands.tunnelDraft(mode.name, mode.personal))
      .then(([draft, version]) => {
        const loaded = draft as TunnelDraft;
        setRevision(version);
        setHops(loaded.hops.map(fromDraft));
        setKeepaliveInterval(loaded.keepaliveInterval ?? null);
        setKeepaliveCount(loaded.keepaliveCount ?? null);
      })
      .catch((e: unknown) => setLoadError(errorText(e)));
  }, [mode]);

  useEffect(() => {
    // Escape closes this editor, not the connection wizard underneath.
    const onKey = (e: KeyboardEvent) => {
      const dialogs = useDialogs.getState();
      if (e.key !== "Escape" || dialogs.trust || dialogs.password || dialogs.interactions.length > 0) return;
      e.stopImmediatePropagation();
      props.onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [props.onClose]);

  const updateHop = (i: number, patch: Partial<HopState>) => {
    setHops((list) => list.map((hop, j) => (j === i ? { ...hop, ...patch } : hop)));
    setStatus({ kind: "idle" });
  };
  const moveHop = (i: number, delta: number) => {
    setHops((list) => {
      const next = [...list];
      const [hop] = next.splice(i, 1);
      if (hop) next.splice(i + delta, 0, hop);
      return next;
    });
    setStatus({ kind: "idle" });
  };

  const draft: TunnelDraft = { personal, name, hops: hops.map(toDraft), keepaliveInterval, keepaliveCount };
  /** A whole number typed in a field, or null when it is empty. */
  const numberOrNull = (text: string) => (text.trim() === "" ? null : Math.max(0, Math.trunc(Number(text))));
  const passwords = hops.map((hop) => hop.password || null);

  async function browseKey(i: number) {
    const path = await open({ multiple: false, directory: false });
    if (typeof path === "string") updateHop(i, { identityFile: path });
  }

  async function test() {
    setStatus({ kind: "testing" });
    try {
      let outcome = await call(commands.testTunnel(draft, passwords));
      if (outcome.kind === "untrusted") {
        if (!(await useDialogs.getState().askTrust(workspace?.workspace ?? null))) {
          setStatus({ kind: "idle" });
          return;
        }
        useApp.getState().setWorkspace(await call(commands.trustWorkspace(workspace?.workspace ?? "")));
        outcome = await call(commands.testTunnel(draft, passwords));
      }
      setStatus(outcome.kind === "connected" ? { kind: "ok" } : { kind: "idle" });
    } catch (e) {
      setStatus({ kind: "error", message: errorText(e) });
    }
  }

  async function save() {
    setBusy(true);
    try {
      if (mode.kind === "edit") {
        await call(commands.updateTunnel(mode.name, mode.personal, draft, passwords, revision ?? ""));
        props.onSaved({ name: mode.name, personal: mode.personal });
      } else {
        props.onSaved(await call(commands.addTunnel(draft, passwords)));
      }
    } catch (e) {
      setStatus({ kind: "error", message: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  async function remove() {
    if (mode.kind !== "edit") return;
    // Two clicks: the first one only asks for confirmation.
    if (!confirmDelete) {
      setConfirmDelete(true);
      return;
    }
    setBusy(true);
    try {
      await call(commands.deleteTunnel(mode.name, mode.personal));
      props.onDeleted();
    } catch (e) {
      setStatus({ kind: "error", message: errorText(e) });
    } finally {
      setBusy(false);
    }
  }

  const methods: SshAuthItem[] = ["agent", "key", "password", "keyboard-interactive"];
  return (
    <div className="modal-backdrop tunnel-backdrop">
      <div className="modal wizard tunnel-editor" role="dialog" aria-modal="true" aria-label={editing ? t("tunnel.editTitle", { name: mode.name }) : t("tunnel.newTitle")}>
        <div className="wizard-header">
          <h2>{editing ? t("tunnel.editTitle", { name: mode.name }) : t("tunnel.newTitle")}</h2>
          <button className="ghost" aria-label={t("close")} onClick={props.onClose}>
            ×
          </button>
        </div>
        <form
          className="connection-form"
          noValidate
          onSubmit={(e) => {
            e.preventDefault();
            void save();
          }}
        >
          <div className="form-scroll">
            {loadError && <p className="error" role="alert">{loadError}</p>}
            <fieldset>
              <legend>{t("sections.general")}</legend>
              <div className="form-pair">
                <label className="field grow">
                  <span>{t("tunnel.name")}</span>
                  <input type="text" value={name} disabled={editing} autoFocus={!editing} placeholder="bastion-eu" onChange={(e) => setName(e.target.value)} />
                </label>
                <div className="form-row" role="radiogroup" aria-label={t("fields.saveIn")}>
                  <span>{t("fields.saveIn")}</span>
                  <label className="check">
                    <input type="radio" name="tunnel-save-in" checked={!personal} disabled={!hasWorkspace || editing} onChange={() => setPersonal(false)} />
                    {t("fields.project")}
                  </label>
                  <label className="check">
                    <input type="radio" name="tunnel-save-in" checked={personal} disabled={editing} onChange={() => setPersonal(true)} />
                    {t("fields.personal")}
                  </label>
                </div>
              </div>
              <p className="muted small">{editing ? t("tunnel.fixedHint") : t("tunnel.nameHint")}</p>
            </fieldset>

            {hops.map((hop, i) => {
              const label = t("tunnel.hop", { number: i + 1 });
              const auth: SshAuthItem[] = hop.method === "custom" ? hop.auth : [hop.method];
              return (
                <fieldset key={i} className="hop" aria-label={label}>
                  <legend>
                    {label}
                    <span className="muted small"> · {i === 0 ? t("tunnel.first") : i === hops.length - 1 ? t("tunnel.last") : t("tunnel.middle")}</span>
                  </legend>
                  <div className="hop-actions">
                    <button type="button" className="ghost small" disabled={i === 0} aria-label={t("tunnel.moveUp", { hop: label })} onClick={() => moveHop(i, -1)}>
                      ↑
                    </button>
                    <button type="button" className="ghost small" disabled={i === hops.length - 1} aria-label={t("tunnel.moveDown", { hop: label })} onClick={() => moveHop(i, 1)}>
                      ↓
                    </button>
                    <button type="button" className="ghost small" disabled={hops.length === 1} aria-label={t("tunnel.removeHop", { hop: label })} onClick={() => setHops((list) => list.filter((_, j) => j !== i))}>
                      ✕
                    </button>
                  </div>
                  <div className="form-pair">
                    <label className="field grow">
                      <span>{t("fields.host")}</span>
                      <input type="text" aria-label={`${label} ${t("fields.host")}`} value={hop.host} onChange={(e) => updateHop(i, { host: e.target.value })} />
                    </label>
                    <label className="field port">
                      <span>{t("fields.port")}</span>
                      <input
                        type="number"
                        min={1}
                        max={65535}
                        aria-label={`${label} ${t("fields.port")}`}
                        value={hop.port ?? ""}
                        placeholder="22"
                        onChange={(e) => updateHop(i, { port: e.target.value ? Number(e.target.value) : null })}
                      />
                    </label>
                    <label className="field grow">
                      <span>{t("fields.user")}</span>
                      <input type="text" aria-label={`${label} ${t("fields.user")}`} value={hop.user} autoComplete="off" onChange={(e) => updateHop(i, { user: e.target.value })} />
                    </label>
                  </div>
                  <label className="field">
                    <span>{t("tunnel.method")}</span>
                    <select aria-label={`${label} ${t("tunnel.method")}`} value={hop.method} onChange={(e) => updateHop(i, { method: e.target.value as Method })}>
                      {methods.map((m) => (
                        <option key={m} value={m}>
                          {t(`tunnel.methods.${m}`)}
                        </option>
                      ))}
                      {hop.auth.length > 1 && <option value="custom">{t("tunnel.methods.custom", { methods: hop.auth.join(", ") })}</option>}
                    </select>
                  </label>
                  {(hop.method === "agent" || hop.method === "key" || hop.method === "custom") && (
                    <div className="form-pair">
                      <label className="field grow">
                        <span>{hop.method === "agent" ? t("tunnel.agentKey") : t("tunnel.keyFile")}</span>
                        <input
                          type="text"
                          aria-label={`${label} ${hop.method === "agent" ? t("tunnel.agentKey") : t("tunnel.keyFile")}`}
                          value={hop.identityFile}
                          placeholder={hop.method === "agent" ? t("tunnel.agentKeyPlaceholder") : t("tunnel.keyFilePlaceholder")}
                          onChange={(e) => updateHop(i, { identityFile: e.target.value })}
                        />
                      </label>
                      <button type="button" onClick={() => void browseKey(i)}>
                        {t("browse")}
                      </button>
                    </div>
                  )}
                  {usesPassword(auth) &&
                    (hop.fixedMode ? (
                      <label className="field">
                        <span>{t("fields.passwordOther")}</span>
                        <input type="text" readOnly value={hop.fixedMode === "env" ? `env:${hop.passwordEnv}` : hop.passwordOther} />
                      </label>
                    ) : (
                      <>
                        <label className="field">
                          <span>{t("fields.password")}</span>
                          <input
                            type="password"
                            aria-label={`${label} ${t("fields.password")}`}
                            value={hop.password}
                            autoComplete="off"
                            placeholder={editing && hop.savePassword ? t("auth.keepPlaceholder") : ""}
                            onChange={(e) => updateHop(i, { password: e.target.value })}
                          />
                        </label>
                        <label className="check">
                          <input type="checkbox" aria-label={`${label} ${t("fields.savePassword")}`} checked={hop.savePassword} onChange={(e) => updateHop(i, { savePassword: e.target.checked })} />
                          {t("fields.savePassword")}
                        </label>
                      </>
                    ))}
                  <p className="muted small">{t(`tunnel.methodHints.${hop.method}`)}</p>
                </fieldset>
              );
            })}
            <button type="button" className="add-hop" onClick={() => setHops((list) => [...list, emptyHop()])}>
              {t("tunnel.addHop")}
            </button>

            <fieldset>
              <legend>{t("tunnel.network")}</legend>
              <div className="form-pair">
                <label className="field">
                  <span>{t("tunnel.keepaliveInterval")}</span>
                  <input
                    type="number"
                    min={0}
                    max={86400}
                    value={keepaliveInterval ?? ""}
                    placeholder="15"
                    onChange={(e) => {
                      setKeepaliveInterval(numberOrNull(e.target.value));
                      setStatus({ kind: "idle" });
                    }}
                  />
                </label>
                <label className="field">
                  <span>{t("tunnel.keepaliveCount")}</span>
                  <input
                    type="number"
                    min={0}
                    max={1000}
                    value={keepaliveCount ?? ""}
                    placeholder="3"
                    onChange={(e) => {
                      setKeepaliveCount(numberOrNull(e.target.value));
                      setStatus({ kind: "idle" });
                    }}
                  />
                </label>
              </div>
              <p className="muted small">{t("tunnel.keepaliveHint")}</p>
            </fieldset>
          </div>

          <div className="wizard-footer">
            <button type="button" onClick={() => void test()} disabled={busy || status.kind === "testing"}>
              {t("tunnel.test")}
            </button>
            <div className="test-result" role="status">
              {status.kind === "testing" && <span className="muted">{t("test.testing")}</span>}
              {status.kind === "ok" && <span className="ok">{t("tunnel.ok", { count: hops.length })}</span>}
              {status.kind === "error" && <span className="error">{status.message}</span>}
            </div>
            {editing && (
              <button type="button" className="danger-text" onClick={() => void remove()} disabled={busy}>
                {confirmDelete ? t("tunnel.confirmDelete") : t("tunnel.delete")}
              </button>
            )}
            <button type="button" onClick={props.onClose} disabled={busy}>
              {t("cancel")}
            </button>
            <button type="submit" className="primary" disabled={busy || Boolean(loadError)}>
              {editing ? t("saveChanges") : t("save")}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
