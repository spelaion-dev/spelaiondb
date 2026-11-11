// Network profile editor (ADR-027), opened from the connection form and Settings → Network.
// An SSH profile has hops in order (the last one reaches the database), each with its server
// and authentication, after a proxy or an AWS SSM session (ADR-050), or one of those alone; a
// Kubernetes profile (ADR-050) has a cluster's settings,
// shared by the connections that use it. The core validates and writes the file; passwords
// go to the keychain, never to the file.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { chooseOpenFile } from "../dialogs";
import { useKeychainAvailable } from "../keychain";
import { call, commands, type HopDraft, type KubernetesDraftItem, type PasswordMode, type ProxyDraft, type SshAuthItem, type SsmDraftItem, type TunnelDraft, type TunnelItem } from "../api";
import { useApp, useDialogs } from "../store";
import { ConnectionTestLog, TestSummary, useConnectionTest } from "./ConnectionTestLog";
import { EMPTY_KUBERNETES, KubernetesTab } from "./KubernetesTab";

/** `kubernetes` starts a new profile as a Kubernetes one (from the Kubernetes tab). */
export type TunnelEditorMode = { kind: "add"; personal: boolean; kubernetes?: boolean } | { kind: "edit"; name: string; personal: boolean };

type Route = "ssh" | "ssm" | "kubernetes";

const EMPTY_SSM: SsmDraftItem = { target: "", region: "", profile: "", aws: "" };

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
  /** Whether the keychain holds a password for it (never the value): only then does an
   * empty field keep one. */
  stored: boolean;
  /** Original reference when it is not keychain/prompt (`env:…`, `op://…`): kept as is. */
  fixedMode: PasswordMode | null;
  passwordEnv: string;
  passwordOther: string;
  password: string;
}

const usesPassword = (auth: SshAuthItem[]) => auth.includes("password") || auth.includes("keyboard-interactive");

function emptyHop(savePassword = true): HopState {
  return {
    host: "",
    port: null,
    user: "",
    method: "agent",
    auth: ["agent"],
    identityFile: "",
    savePassword,
    stored: false,
    fixedMode: null,
    passwordEnv: "",
    passwordOther: "",
    password: "",
  };
}

function fromDraft(hop: HopDraft, stored: boolean): HopState {
  const method: Method = hop.auth.length === 1 && hop.auth[0] ? hop.auth[0] : "custom";
  return {
    host: hop.host,
    port: hop.port ?? null,
    user: hop.user,
    method,
    auth: hop.auth,
    identityFile: hop.identityFile,
    savePassword: hop.passwordMode !== "prompt",
    stored,
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

/** The proxy section (ADR-027). Its password, if typed, travels apart from the draft. */
interface ProxyState {
  enabled: boolean;
  kind: "socks5" | "http";
  host: string;
  port: number | null;
  user: string;
  /** Keychain when true, asked when false. */
  savePassword: boolean;
  /** Whether the keychain holds a password for it (never the value): only then does an
   * empty field keep one. */
  stored: boolean;
  /** Original reference when it is not keychain/prompt (`env:…`, `op://…`): kept as is. */
  fixedMode: PasswordMode | null;
  passwordEnv: string;
  passwordOther: string;
  password: string;
}

const noProxy: ProxyState = { enabled: false, kind: "socks5", host: "", port: null, user: "", savePassword: true, stored: false, fixedMode: null, passwordEnv: "", passwordOther: "", password: "" };

function proxyFromDraft(proxy: ProxyDraft | null | undefined, stored: boolean): ProxyState {
  if (!proxy) return noProxy;
  return {
    enabled: true,
    kind: proxy.kind,
    host: proxy.host,
    port: proxy.port ?? null,
    user: proxy.user,
    savePassword: proxy.passwordMode !== "prompt",
    stored,
    fixedMode: proxy.passwordMode === "env" || proxy.passwordMode === "other" ? proxy.passwordMode : null,
    passwordEnv: proxy.passwordEnv,
    passwordOther: proxy.passwordOther,
    password: "",
  };
}

function proxyToDraft(proxy: ProxyState): ProxyDraft | null {
  if (!proxy.enabled) return null;
  const passwordMode: PasswordMode = !proxy.user.trim() ? "none" : (proxy.fixedMode ?? (proxy.savePassword ? "keychain" : "prompt"));
  return { kind: proxy.kind, host: proxy.host, port: proxy.port, user: proxy.user, passwordMode, passwordEnv: proxy.passwordEnv, passwordOther: proxy.passwordOther };
}

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
  const [connectTimeout, setConnectTimeout] = useState<number | null>(null);
  const [proxy, setProxy] = useState<ProxyState>(noProxy);
  const [route, setRoute] = useState<Route>(mode.kind === "add" && mode.kubernetes ? "kubernetes" : "ssh");
  const [kubernetes, setKubernetes] = useState<KubernetesDraftItem>(EMPTY_KUBERNETES);
  const [ssm, setSsm] = useState<SsmDraftItem>(EMPTY_SSM);
  const [revision, setRevision] = useState<string | null>(null);
  // An edited profile is shown only once its file is read: until then the route would be the
  // default one (SSH, with no hops) and a save would write that over the file.
  const loaded = !editing || revision !== null;
  const [loadError, setLoadError] = useState<string | null>(null);
  const [status, setStatus] = useState<{ kind: "idle" } | { kind: "error"; message: string }>({ kind: "idle" });
  const log = useConnectionTest();
  const [busy, setBusy] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState(false);
  // Without a system keychain a new tunnel asks for its passwords; an edited one keeps what
  // its file says (a repository file is the team's) unless the user unticks it.
  const keychain = useKeychainAvailable();
  /** An edited password field's hint: keep the stored password, or there is none to keep. */
  const storedPlaceholder = (stored: boolean) =>
    stored ? t("auth.keepPlaceholder") : keychain === false ? "" : t("auth.notStoredPlaceholder");
  useEffect(() => {
    if (keychain !== false || editing) return;
    setHops((list) => list.map((hop) => ({ ...hop, savePassword: false })));
    setProxy((current) => ({ ...current, savePassword: false }));
  }, [keychain, editing]);

  useEffect(() => {
    if (mode.kind !== "edit") return;
    call(commands.tunnelDraft(mode.name, mode.personal))
      .then((edited) => {
        const loaded = edited.draft as TunnelDraft;
        setRevision(edited.revision);
        setHops(loaded.hops.map((hop, i) => fromDraft(hop, edited.hopPasswordsStored[i] ?? false)));
        setKeepaliveInterval(loaded.keepaliveInterval ?? null);
        setKeepaliveCount(loaded.keepaliveCount ?? null);
        setConnectTimeout(loaded.connectTimeout ?? null);
        setProxy(proxyFromDraft(loaded.proxy, edited.proxyPasswordStored));
        if (loaded.ssm) {
          setRoute("ssm");
          setSsm(loaded.ssm);
        }
        if (loaded.kubernetes) {
          setRoute("kubernetes");
          setKubernetes(loaded.kubernetes);
          setHops([emptyHop()]);
        }
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

  const kube = route === "kubernetes";
  const viaSsm = route === "ssm";
  const draft: TunnelDraft = kube
    ? { personal, name, hops: [], keepaliveInterval: null, keepaliveCount: null, connectTimeout: null, proxy: null, ssm: null, kubernetes }
    : {
        personal,
        name,
        hops: hops.map(toDraft),
        keepaliveInterval,
        keepaliveCount,
        connectTimeout,
        proxy: viaSsm ? null : proxyToDraft(proxy),
        ssm: viaSsm ? ssm : null,
        kubernetes: null,
      };
  const proxyPassword = route === "ssh" && proxy.enabled && proxy.password ? proxy.password : null;
  const updateSsm = (patch: Partial<SsmDraftItem>) => {
    setSsm((current) => ({ ...current, ...patch }));
    setStatus({ kind: "idle" });
  };

  /** A repository's kubectl settings are only used once it is trusted (ADR-022). */
  async function chooseRoute(next: Route) {
    if (next === "kubernetes" && !personal && !useApp.getState().workspace?.trusted) {
      const shown = useApp.getState().workspace?.workspace ?? null;
      try {
        if (!(await useDialogs.getState().askTrust(shown))) return;
        useApp.getState().setWorkspace(await call(commands.trustWorkspace(shown ?? "")));
      } catch (e) {
        setStatus({ kind: "error", message: errorText(e) });
        return;
      }
    }
    setRoute(next);
    if (next === "ssh" && hops.length === 0 && !proxy.enabled) setHops([emptyHop()]);
    // AWS SSM may reach the database alone: a new profile starts without hops.
    if (next === "ssm" && hops.length === 1 && !hops[0]?.host && !hops[0]?.user) setHops([]);
    setStatus({ kind: "idle" });
  }
  const updateProxy = (patch: Partial<ProxyState>) => {
    setProxy((current) => ({ ...current, ...patch }));
    setStatus({ kind: "idle" });
  };
  /** A whole number typed in a field, or null when it is empty. */
  const numberOrNull = (text: string) => (text.trim() === "" ? null : Math.max(0, Math.trunc(Number(text))));
  const passwords = kube ? [] : hops.map((hop) => hop.password || null);
  // Any change to the profile makes the last test's log stale (and stops a running one).
  const testedKey = JSON.stringify([draft, passwords, proxyPassword]);
  useEffect(() => log.clear(), [testedKey]);

  async function browseKey(i: number) {
    const path = await chooseOpenFile(undefined, [], hops[i]?.identityFile);
    if (path) updateHop(i, { identityFile: path });
  }

  /** Tests the profile step by step (ADR-032): the proxy and each hop, in the log. */
  async function test() {
    setStatus({ kind: "idle" });
    try {
      const attempt = () => log.run((id, onStep) => call(commands.testTunnel(draft, passwords, proxyPassword, id, onStep)));
      let outcome = await attempt();
      if (outcome.kind === "untrusted") {
        if (!(await useDialogs.getState().askTrust(workspace?.workspace ?? null))) {
          log.clear();
          return;
        }
        useApp.getState().setWorkspace(await call(commands.trustWorkspace(workspace?.workspace ?? "")));
        outcome = await attempt();
      }
      if (outcome.kind === "tested") log.finish(outcome.report);
      else log.clear();
    } catch (e) {
      log.clear();
      setStatus({ kind: "error", message: errorText(e) });
    }
  }

  async function save() {
    setBusy(true);
    try {
      if (mode.kind === "edit") {
        await call(commands.updateTunnel(mode.name, mode.personal, draft, passwords, proxyPassword, revision ?? ""));
        props.onSaved({ name: mode.name, personal: mode.personal, kubernetes: kube ? kubernetes : null });
      } else {
        props.onSaved(await call(commands.addTunnel(draft, passwords, proxyPassword)));
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
            {!loaded && !loadError && <p className="muted" role="status">{t("tunnel.loading")}</p>}
            {loaded && (
            <>
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
              <div className="form-row" role="radiogroup" aria-label={t("tunnel.route")}>
                <span>{t("tunnel.route")}</span>
                <label className="check">
                  <input type="radio" name="tunnel-route" checked={route === "ssh"} onChange={() => void chooseRoute("ssh")} />
                  {t("tunnel.routeSsh")}
                </label>
                <label className="check">
                  <input type="radio" name="tunnel-route" checked={viaSsm} onChange={() => void chooseRoute("ssm")} />
                  {t("tunnel.routeSsm")}
                </label>
                <label className="check">
                  <input type="radio" name="tunnel-route" checked={kube} onChange={() => void chooseRoute("kubernetes")} />
                  {t("tunnel.routeKubernetes")}
                </label>
              </div>
            </fieldset>

            {kube && (
              <KubernetesTab
                mode="profile"
                personal={personal}
                editing={editing ? mode.name : null}
                trusted={Boolean(workspace?.trusted)}
                value={kubernetes}
                onChange={(next) => {
                  setKubernetes(next);
                  setStatus({ kind: "idle" });
                }}
              />
            )}
            {viaSsm && (
              <fieldset aria-label={t("tunnel.ssm")}>
                <legend>{t("tunnel.ssm")}</legend>
                <div className="form-pair">
                  <label className="field grow">
                    <span>{t("tunnel.ssmTarget")}</span>
                    <input type="text" value={ssm.target} autoComplete="off" spellCheck={false} placeholder="i-0123456789abcdef0" onChange={(e) => updateSsm({ target: e.target.value })} />
                  </label>
                  <label className="field">
                    <span>{t("tunnel.ssmRegion")}</span>
                    <input type="text" value={ssm.region} autoComplete="off" spellCheck={false} placeholder={t("tunnel.ssmRegionPlaceholder")} onChange={(e) => updateSsm({ region: e.target.value })} />
                  </label>
                </div>
                <div className="form-pair">
                  <label className="field grow">
                    <span>{t("tunnel.ssmProfile")}</span>
                    <input type="text" value={ssm.profile} autoComplete="off" spellCheck={false} placeholder={t("tunnel.ssmProfilePlaceholder")} onChange={(e) => updateSsm({ profile: e.target.value })} />
                  </label>
                  <label className="field grow">
                    <span>{t("tunnel.ssmAws")}</span>
                    <input type="text" value={ssm.aws} autoComplete="off" spellCheck={false} placeholder="aws" onChange={(e) => updateSsm({ aws: e.target.value })} />
                  </label>
                </div>
                <p className="muted small">{t("tunnel.ssmHint")}</p>
                {hops.length > 0 && <p className="muted small">{t("tunnel.ssmHopsHint")}</p>}
              </fieldset>
            )}
            {!kube && (
              <>

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
                    <button type="button" className="ghost small" disabled={hops.length === 1 && !proxy.enabled && !viaSsm} aria-label={t("tunnel.removeHop", { hop: label })} onClick={() => setHops((list) => list.filter((_, j) => j !== i))}>
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
                            placeholder={editing && hop.savePassword ? storedPlaceholder(hop.stored) : ""}
                            onChange={(e) => updateHop(i, { password: e.target.value })}
                          />
                        </label>
                        <label className="check">
                          <input
                            type="checkbox"
                            aria-label={`${label} ${t("fields.savePassword")}`}
                            checked={hop.savePassword}
                            disabled={keychain === false && !hop.savePassword}
                            onChange={(e) => updateHop(i, { savePassword: e.target.checked })}
                          />
                          {t("fields.savePassword")}
                        </label>
                        {keychain === false && <p className="muted small">{t("auth.noKeychainHint")}</p>}
                      </>
                    ))}
                  <p className="muted small">{t(`tunnel.methodHints.${hop.method}`)}</p>
                </fieldset>
              );
            })}
            <button type="button" className="add-hop" onClick={() => setHops((list) => [...list, emptyHop(keychain !== false)])}>
              {t("tunnel.addHop")}
            </button>

            {!viaSsm && (
            <fieldset aria-label={t("tunnel.proxy")}>
              <legend>{t("tunnel.proxy")}</legend>
              <label className="check">
                <input type="checkbox" checked={proxy.enabled} onChange={(e) => updateProxy({ enabled: e.target.checked })} />
                {t("tunnel.useProxy")}
              </label>
              {proxy.enabled && (
                <>
                  <div className="form-pair">
                    <label className="field">
                      <span>{t("tunnel.proxyType")}</span>
                      <select value={proxy.kind} onChange={(e) => updateProxy({ kind: e.target.value as ProxyState["kind"] })}>
                        <option value="socks5">SOCKS5</option>
                        <option value="http">HTTP (CONNECT)</option>
                      </select>
                    </label>
                    <label className="field grow">
                      <span>{t("tunnel.proxyHost")}</span>
                      <input type="text" value={proxy.host} placeholder="proxy.example.com" onChange={(e) => updateProxy({ host: e.target.value })} />
                    </label>
                    <label className="field port">
                      <span>{t("tunnel.proxyPort")}</span>
                      <input type="number" min={1} max={65535} value={proxy.port ?? ""} placeholder={proxy.kind === "socks5" ? "1080" : "3128"} onChange={(e) => updateProxy({ port: e.target.value ? Number(e.target.value) : null })} />
                    </label>
                  </div>
                  <div className="form-pair">
                    <label className="field grow">
                      <span>{t("tunnel.proxyUser")}</span>
                      <input type="text" value={proxy.user} autoComplete="off" placeholder={t("tunnel.proxyUserPlaceholder")} onChange={(e) => updateProxy({ user: e.target.value })} />
                    </label>
                    {proxy.user.trim() &&
                      (proxy.fixedMode ? (
                        <label className="field grow">
                          <span>{t("tunnel.proxyPassword")}</span>
                          <input type="text" readOnly value={proxy.fixedMode === "env" ? `env:${proxy.passwordEnv}` : proxy.passwordOther} />
                        </label>
                      ) : (
                        <label className="field grow">
                          <span>{t("tunnel.proxyPassword")}</span>
                          <input
                            type="password"
                            value={proxy.password}
                            autoComplete="off"
                            placeholder={editing && proxy.savePassword ? storedPlaceholder(proxy.stored) : ""}
                            onChange={(e) => updateProxy({ password: e.target.value })}
                          />
                        </label>
                      ))}
                  </div>
                  {proxy.user.trim() && !proxy.fixedMode && (
                    <label className="check">
                      <input
                        type="checkbox"
                        aria-label={t("tunnel.proxySavePassword")}
                        checked={proxy.savePassword}
                        disabled={keychain === false && !proxy.savePassword}
                        onChange={(e) => updateProxy({ savePassword: e.target.checked })}
                      />
                      {t("tunnel.proxySavePassword")}
                    </label>
                  )}
                  {proxy.user.trim() && !proxy.fixedMode && keychain === false && <p className="muted small">{t("auth.noKeychainHint")}</p>}
                  <p className="muted small">{hops.length === 0 ? t("tunnel.proxyAloneHint") : t("tunnel.proxyHint")}</p>
                </>
              )}
            </fieldset>
            )}

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
                <label className="field">
                  <span>{t("tunnel.connectTimeout")}</span>
                  <input
                    type="number"
                    min={1}
                    max={600}
                    value={connectTimeout ?? ""}
                    placeholder="15"
                    onChange={(e) => {
                      setConnectTimeout(numberOrNull(e.target.value));
                      setStatus({ kind: "idle" });
                    }}
                  />
                </label>
              </div>
              <p className="muted small">{t("tunnel.keepaliveHint")}</p>
              <p className="muted small">{t("tunnel.connectTimeoutHint")}</p>
            </fieldset>
              </>
            )}
            </>
            )}
          </div>

          {log.steps && <ConnectionTestLog steps={log.steps} report={log.report} running={log.running} onStop={log.stop} onClose={log.clear} />}
          <div className="wizard-footer">
            {!kube && (
              <button type="button" onClick={() => void test()} disabled={busy || log.running || !loaded}>
                {t("tunnel.test")}
              </button>
            )}
            <div className="test-result" role="status">
              <TestSummary
                steps={log.steps}
                report={log.report}
                running={log.running}
                okText={() => (hops.length === 0 ? (viaSsm ? t("tunnel.okSsm") : t("tunnel.okProxy")) : t("tunnel.ok", { count: hops.length }))}
              />
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
            <button type="submit" className="primary" disabled={busy || !loaded || Boolean(loadError)}>
              {editing ? t("saveChanges") : t("save")}
            </button>
          </div>
        </form>
      </div>
    </div>
  );
}
