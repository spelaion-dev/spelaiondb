// "Add connection" wizard: 1) pick a database from the catalog, grouped by category;
// 2) fill in the engine's form, test it and save it. Validation and writing the file live
// in the core: this only collects what the user typed.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import { open } from "@tauri-apps/plugin-dialog";
import {
  call,
  commands,
  type ConnectionDraft,
  type EngineItem,
  type PasswordMode,
  type TlsModeItem,
  type TunnelItem,
} from "../api";
import { useApp, useDialogs } from "../store";
import { refreshConnections } from "./Sidebar";

const CATEGORIES = ["relational", "analytical", "nosql"] as const;
const TLS_MODES: TlsModeItem[] = ["disable", "prefer", "require", "verify-ca", "verify-full"];

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function AddConnection() {
  const { t } = useTranslation("connections");
  const close = () => useApp.getState().setAddConnectionOpen(false);
  const [engines, setEngines] = useState<EngineItem[]>([]);
  const [engine, setEngine] = useState<EngineItem | null>(null);

  useEffect(() => {
    void commands.engineCatalog().then(setEngines);
    const onKey = (e: KeyboardEvent) => {
      // Dialogs opened on top (trust, SSH fingerprint…) handle their own Escape.
      const dialogs = useDialogs.getState();
      if (e.key === "Escape" && !dialogs.trust && !dialogs.password && dialogs.interactions.length === 0) close();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  return (
    <div className="modal-backdrop wizard-backdrop">
      <div className="modal wizard" role="dialog" aria-modal="true" aria-label={t("title")}>
        <div className="wizard-header">
          <h2>
            {engine && <EngineBadge engine={engine} />}
            {engine ? t("formTitle", { engine: engine.name }) : t("title")}
          </h2>
          <button className="ghost" aria-label={t("close")} onClick={close}>
            ×
          </button>
        </div>
        {engine ? (
          <ConnectionForm engine={engine} onBack={() => setEngine(null)} onCancel={close} onSaved={close} />
        ) : (
          <EnginePicker engines={engines} onPick={setEngine} />
        )}
      </div>
    </div>
  );
}

// Two-letter badges, matching the sidebar (PG, SQ) and unique within the catalog.
const BADGES: Record<string, string> = {
  postgres: "PG",
  sqlite: "SQ",
  mysql: "MY",
  mariadb: "MA",
  sqlserver: "MS",
  oracle: "OR",
  cockroachdb: "CR",
  duckdb: "DK",
  clickhouse: "CH",
  snowflake: "SF",
  bigquery: "BQ",
  mongodb: "MG",
  redis: "RD",
  cassandra: "CA",
};

function EngineBadge({ engine }: { engine: EngineItem }) {
  return (
    <span className={`engine engine-${engine.id}`} aria-hidden="true">
      {BADGES[engine.id] ?? engine.name.slice(0, 2).toUpperCase()}
    </span>
  );
}

function EnginePicker({ engines, onPick }: { engines: EngineItem[]; onPick(engine: EngineItem): void }) {
  const { t } = useTranslation("connections");
  const [search, setSearch] = useState("");
  const query = search.trim().toLowerCase();
  const visible = engines.filter((e) => e.name.toLowerCase().includes(query));
  return (
    <div className="engine-picker">
      <p className="muted">{t("pickHint")}</p>
      <input
        type="search"
        autoFocus
        placeholder={t("search")}
        aria-label={t("search")}
        value={search}
        onChange={(e) => setSearch(e.target.value)}
      />
      <div className="engine-groups">
        {CATEGORIES.map((category) => {
          const items = visible.filter((e) => e.category === category);
          if (items.length === 0) return null;
          return (
            <section key={category} aria-labelledby={`engines-${category}`}>
              <h3 id={`engines-${category}`}>{t(`categories.${category}`)}</h3>
              <div className="engine-grid">
                {items.map((e) => (
                  <button
                    key={e.id}
                    className="engine-card"
                    disabled={!e.available}
                    onClick={() => onPick(e)}
                    title={e.available ? e.name : t("comingSoonHint", { engine: e.name })}
                  >
                    <EngineBadge engine={e} />
                    <span className="engine-card-name">{e.name}</span>
                    {!e.available && <span className="engine-card-soon">{t("comingSoon")}</span>}
                  </button>
                ))}
              </div>
            </section>
          );
        })}
        {visible.length === 0 && <p className="muted">{t("noMatches")}</p>}
      </div>
    </div>
  );
}

type TestState =
  | { kind: "idle" }
  | { kind: "testing" }
  | { kind: "ok"; version: string }
  | { kind: "error"; message: string };

/** How the form asks for credentials, like DBeaver's "Authentication" selector. */
type AuthMethod = "password" | "env" | "none";
type FormTab = "main" | "ssl" | "ssh" | "options";

function Field({ label, children, className }: { label: string; children: React.ReactNode; className?: string }) {
  return (
    <label className={`field ${className ?? ""}`}>
      <span>{label}</span>
      {children}
    </label>
  );
}

function ConnectionForm(props: { engine: EngineItem; onBack(): void; onCancel(): void; onSaved(): void }) {
  const { engine } = props;
  const { t } = useTranslation("connections");
  const workspace = useApp((s) => s.workspace);
  const hasWorkspace = Boolean(workspace?.workspace);
  const network = engine.access === "network";
  const [tab, setTab] = useState<FormTab>("main");
  const [tunnels, setTunnels] = useState<TunnelItem[]>([]);
  const [auth, setAuth] = useState<AuthMethod>(network ? "password" : "none");
  const [savePassword, setSavePassword] = useState(true);
  const [useTunnel, setUseTunnel] = useState(false);
  const [password, setPassword] = useState("");
  const [test, setTest] = useState<TestState>({ kind: "idle" });
  const [saving, setSaving] = useState(false);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Omit<ConnectionDraft, "passwordMode" | "tunnel">>({
    personal: !hasWorkspace,
    folder: "",
    name: "",
    engine: engine.id,
    env: "",
    host: network ? "localhost" : "",
    port: null,
    database: "",
    user: "",
    passwordEnv: "",
    tlsMode: "prefer",
    tlsCa: "",
    readOnly: false,
    mcpExposed: false,
  });
  const [tunnel, setTunnel] = useState("");

  useEffect(() => {
    if (!network) return;
    void call(commands.listTunnels())
      .then((list) => {
        setTunnels(list);
        setTunnel((current) => current || (list[0]?.name ?? ""));
      })
      .catch(() => setTunnels([]));
  }, [network]);

  const changed = () => {
    setTest({ kind: "idle" });
    setSaveError(null);
  };
  const update = (patch: Partial<typeof draft>) => {
    setDraft((d) => ({ ...d, ...patch }));
    changed();
  };

  // What the core receives: the form's choices mapped to the configuration format.
  const passwordMode: PasswordMode = !network || auth === "none" ? "none" : auth === "env" ? "env" : savePassword ? "keychain" : "prompt";
  const request: ConnectionDraft = { ...draft, passwordMode, tunnel: network && useTunnel ? tunnel : "" };
  const typedPassword = auth === "password" ? password : "";
  const busy = saving || test.kind === "testing";

  async function browse(field: "database" | "tlsCa") {
    const path = await open({
      multiple: false,
      directory: false,
      filters:
        field === "database"
          ? [{ name: "SQLite", extensions: ["db", "sqlite", "sqlite3", "db3"] }, { name: t("allFiles"), extensions: ["*"] }]
          : [{ name: t("certificates"), extensions: ["pem", "crt", "cer"] }, { name: t("allFiles"), extensions: ["*"] }],
    });
    if (typeof path === "string") update({ [field]: path });
  }

  async function runTest() {
    setTest({ kind: "testing" });
    try {
      let outcome = await call(commands.testConnection(request, typedPassword || null));
      if (outcome.kind === "untrusted") {
        if (!(await useDialogs.getState().askTrust(workspace?.workspace ?? null))) {
          setTest({ kind: "idle" });
          return;
        }
        useApp.getState().setWorkspace(await call(commands.trustWorkspace()));
        outcome = await call(commands.testConnection(request, typedPassword || null));
      }
      switch (outcome.kind) {
        case "connected":
          setTest({ kind: "ok", version: outcome.serverVersion });
          break;
        case "passwordRequired":
          setTest({ kind: "error", message: t("test.passwordRequired") });
          break;
        case "authFailed":
          setTest({ kind: "error", message: outcome.message });
          break;
        case "untrusted":
          setTest({ kind: "idle" });
          break;
      }
    } catch (e) {
      setTest({ kind: "error", message: errorText(e) });
    }
  }

  async function save() {
    setSaving(true);
    setSaveError(null);
    try {
      // Only a saved password reaches the keychain; otherwise it was just for the test.
      await call(commands.addConnection(request, passwordMode === "keychain" && password ? password : null));
      await refreshConnections();
      props.onSaved();
    } catch (e) {
      setSaveError(errorText(e));
    } finally {
      setSaving(false);
    }
  }

  const tabs: FormTab[] = network ? ["main", "ssl", "ssh", "options"] : ["main", "options"];
  return (
    <form
      className="connection-form"
      noValidate
      onSubmit={(e) => {
        e.preventDefault();
        void save();
      }}
    >
      <div className="form-top">
        <Field label={t("fields.name")} className="grow">
          <input type="text" autoFocus value={draft.name} placeholder={t("fields.namePlaceholder", { engine: engine.name })} onChange={(e) => update({ name: e.target.value })} />
        </Field>
      </div>
      <div className="form-tabs" role="tablist">
        {tabs.map((id) => (
          <button key={id} type="button" role="tab" aria-selected={tab === id} aria-controls={`connection-tab-${id}`} onClick={() => setTab(id)}>
            {t(`tabs.${id}`)}
          </button>
        ))}
      </div>

      <div className="form-scroll">
        <div id="connection-tab-main" role="tabpanel" hidden={tab !== "main"}>
          {network ? (
            <>
              <fieldset>
                <legend>{t("sections.server")}</legend>
                <div className="form-pair">
                  <Field label={t("fields.host")} className="grow">
                    <input type="text" value={draft.host} onChange={(e) => update({ host: e.target.value })} />
                  </Field>
                  <Field label={t("fields.port")} className="port">
                    <input
                      type="number"
                      min={1}
                      max={65535}
                      value={draft.port ?? ""}
                      placeholder={engine.defaultPort ? String(engine.defaultPort) : ""}
                      onChange={(e) => update({ port: e.target.value ? Number(e.target.value) : null })}
                    />
                  </Field>
                </div>
                <Field label={t("fields.database")}>
                  <input type="text" value={draft.database} onChange={(e) => update({ database: e.target.value })} />
                </Field>
              </fieldset>
              <fieldset>
                <legend>{t("sections.authentication")}</legend>
                <Field label={t("fields.authentication")}>
                  <select aria-label={t("fields.authentication")} value={auth} onChange={(e) => { setAuth(e.target.value as AuthMethod); changed(); }}>
                    <option value="password">{t("auth.password")}</option>
                    <option value="env">{t("auth.env")}</option>
                    <option value="none">{t("auth.none")}</option>
                  </select>
                </Field>
                <Field label={t("fields.user")}>
                  <input type="text" value={draft.user} autoComplete="off" onChange={(e) => update({ user: e.target.value })} />
                </Field>
                {auth === "password" && (
                  <>
                    <Field label={t("fields.password")}>
                      <input type="password" value={password} autoComplete="off" onChange={(e) => { setPassword(e.target.value); changed(); }} />
                    </Field>
                    <label className="check">
                      <input type="checkbox" checked={savePassword} onChange={(e) => { setSavePassword(e.target.checked); changed(); }} />
                      {t("fields.savePassword")}
                    </label>
                    <p className="muted small">{savePassword ? t("auth.savedHint") : t("auth.askHint")}</p>
                  </>
                )}
                {auth === "env" && (
                  <>
                    <Field label={t("fields.passwordEnv")}>
                      <input type="text" value={draft.passwordEnv} placeholder="PGPASSWORD" onChange={(e) => update({ passwordEnv: e.target.value })} />
                    </Field>
                    <p className="muted small">{t("auth.envHint")}</p>
                  </>
                )}
              </fieldset>
            </>
          ) : (
            <fieldset>
              <legend>{t("sections.file")}</legend>
              <div className="form-pair">
                <Field label={t("fields.file")} className="grow">
                  <input type="text" value={draft.database} onChange={(e) => update({ database: e.target.value })} />
                </Field>
                <button type="button" onClick={() => void browse("database")}>
                  {t("browse")}
                </button>
              </div>
              <p className="muted small">{t("fields.fileHint")}</p>
            </fieldset>
          )}
        </div>

        {network && (
          <div id="connection-tab-ssl" role="tabpanel" hidden={tab !== "ssl"}>
            <fieldset>
              <legend>{t("sections.ssl")}</legend>
              <Field label={t("fields.tlsMode")}>
                <select aria-label={t("fields.tlsMode")} value={draft.tlsMode} onChange={(e) => update({ tlsMode: e.target.value as TlsModeItem })}>
                  {TLS_MODES.map((mode) => (
                    <option key={mode} value={mode}>
                      {t(`tls.${mode}`)}
                    </option>
                  ))}
                </select>
              </Field>
              <div className="form-pair">
                <Field label={t("fields.tlsCa")} className="grow">
                  <input
                    type="text"
                    value={draft.tlsCa}
                    disabled={draft.tlsMode !== "verify-ca" && draft.tlsMode !== "verify-full"}
                    placeholder={t("fields.tlsCaPlaceholder")}
                    onChange={(e) => update({ tlsCa: e.target.value })}
                  />
                </Field>
                <button type="button" disabled={draft.tlsMode !== "verify-ca" && draft.tlsMode !== "verify-full"} onClick={() => void browse("tlsCa")}>
                  {t("browse")}
                </button>
              </div>
              <p className="muted small">{t("fields.tlsHint")}</p>
            </fieldset>
          </div>
        )}

        {network && (
          <div id="connection-tab-ssh" role="tabpanel" hidden={tab !== "ssh"}>
            <fieldset>
              <legend>{t("sections.ssh")}</legend>
              <label className="check">
                <input type="checkbox" checked={useTunnel} disabled={tunnels.length === 0} onChange={(e) => { setUseTunnel(e.target.checked); changed(); }} />
                {t("fields.useTunnel")}
              </label>
              <Field label={t("fields.tunnel")}>
                <select aria-label={t("fields.tunnel")} value={tunnel} disabled={!useTunnel} onChange={(e) => { setTunnel(e.target.value); changed(); }}>
                  {tunnels.map((item) => (
                    <option key={`${item.personal}:${item.name}`} value={item.name}>
                      {item.name}
                      {item.personal ? ` (${t("fields.personal")})` : ""}
                    </option>
                  ))}
                </select>
              </Field>
              <p className="muted small">{tunnels.length === 0 ? t("fields.noTunnelsHint") : t("fields.tunnelHint")}</p>
            </fieldset>
          </div>
        )}

        <div id="connection-tab-options" role="tabpanel" hidden={tab !== "options"}>
          <fieldset>
            <legend>{t("sections.organization")}</legend>
            <div className="form-row" role="radiogroup" aria-label={t("fields.saveIn")}>
              <span>{t("fields.saveIn")}</span>
              <label className="check">
                <input type="radio" name="save-in" checked={!draft.personal} disabled={!hasWorkspace} onChange={() => update({ personal: false })} />
                {t("fields.project")}
              </label>
              <label className="check">
                <input type="radio" name="save-in" checked={draft.personal} onChange={() => update({ personal: true })} />
                {t("fields.personal")}
              </label>
            </div>
            <p className="muted small">{draft.personal ? t("fields.personalHint") : t("fields.projectHint")}</p>
            <div className="form-pair">
              <Field label={t("fields.folder")} className="grow">
                <input type="text" value={draft.folder} placeholder={t("fields.folderPlaceholder")} onChange={(e) => update({ folder: e.target.value })} />
              </Field>
              <Field label={t("fields.env")} className="grow">
                <input type="text" list="env-labels" value={draft.env} placeholder="dev, staging, prod…" onChange={(e) => update({ env: e.target.value })} />
                <datalist id="env-labels">
                  <option value="dev" />
                  <option value="staging" />
                  <option value="prod" />
                </datalist>
              </Field>
            </div>
          </fieldset>
          <fieldset>
            <legend>{t("sections.safety")}</legend>
            <label className="check">
              <input type="checkbox" checked={draft.readOnly} onChange={(e) => update({ readOnly: e.target.checked })} />
              {t("fields.readOnly")}
            </label>
            <label className="check">
              <input type="checkbox" checked={draft.mcpExposed} onChange={(e) => update({ mcpExposed: e.target.checked })} />
              {t("fields.mcpExposed")}
            </label>
            <p className="muted small">{t("fields.mcpHint")}</p>
          </fieldset>
        </div>
      </div>

      <div className="wizard-footer">
        <button type="button" onClick={() => void runTest()} disabled={busy}>
          {t("test.button")}
        </button>
        <div className="test-result" role="status">
          {test.kind === "testing" && <span className="muted">{t("test.testing")}</span>}
          {test.kind === "ok" && <span className="ok">{t("test.ok", { version: test.version })}</span>}
          {test.kind === "error" && <span className="error">{test.message}</span>}
          {saveError && <span className="error">{saveError}</span>}
        </div>
        <button type="button" onClick={props.onBack} disabled={busy}>
          {t("back")}
        </button>
        <button type="button" onClick={props.onCancel} disabled={saving}>
          {t("cancel")}
        </button>
        <button type="submit" className="primary" disabled={busy}>
          {saving ? t("saving") : t("save")}
        </button>
      </div>
    </form>
  );
}
