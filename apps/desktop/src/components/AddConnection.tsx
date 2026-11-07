// "Add connection" wizard: 1) pick a database from the catalog, grouped by category;
// 2) fill in the engine's form, test it and save it. Validation and writing the file live
// in the core: this only collects what the user typed.

import { useCallback, useEffect, useState } from "react";
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
  type UrlFields,
} from "../api";
import { type ConnectionDialog, useApp, useDialogs } from "../store";
import { EngineIcon } from "./EngineIcon";
import { refreshConnections } from "./Sidebar";
import { chooseSavePath, openFilters } from "../dialogs";
import { reviewChanges } from "../connect";
import { DbeaverImport } from "./DbeaverImport";
import { TunnelEditor, type TunnelEditorMode } from "./TunnelEditor";
import { EMPTY_KUBERNETES, KubernetesTab } from "./KubernetesTab";

const CATEGORIES = ["relational", "analytical", "nosql"] as const;
const TLS_MODES: TlsModeItem[] = ["disable", "prefer", "require", "verify-ca", "verify-full"];

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** File types of the embedded engines, for the open and save dialogs. */
const DATABASE_FILES: Record<string, { name: string; extensions: string[]; fallback: string }> = {
  sqlite: { name: "SQLite", extensions: ["sqlite", "db", "sqlite3", "db3"], fallback: "database.sqlite" },
  duckdb: { name: "DuckDB", extensions: ["duckdb", "db", "ddb"], fallback: "database.duckdb" },
};

export function AddConnection({ dialog }: { dialog: ConnectionDialog }) {
  const { t } = useTranslation("connections");
  const close = () => useApp.getState().setConnectionDialog(null);
  const editing = dialog.kind === "edit" ? dialog.id : null;
  const [engines, setEngines] = useState<EngineItem[]>([]);
  const [engine, setEngine] = useState<EngineItem | null>(null);
  const [prefill, setPrefill] = useState<UrlFields | null>(null);
  const [importing, setImporting] = useState(false);
  const [initial, setInitial] = useState<ConnectionDraft | null>(null);
  const [revision, setRevision] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);

  useEffect(() => {
    void commands.engineCatalog().then(async (catalog) => {
      setEngines(catalog);
      if (!editing) return;
      try {
        let access = await call(commands.connectionEditorAccess(editing));
        if (access.kind === "untrusted") {
          const shown = useApp.getState().workspace?.workspace ?? null;
          if (!(await useDialogs.getState().askTrust(shown))) { close(); return; }
          useApp.getState().setWorkspace(await call(commands.trustWorkspace(shown ?? "")));
          access = await call(commands.connectionEditorAccess(editing));
        }
        if (access.kind === "changed") {
          const accepted = await useDialogs.getState().askChange({ item: access.item, isNew: access.new, fields: access.fields });
          if (!accepted) { close(); return; }
          await call(commands.acceptChange(access.root, access.item, access.new, access.fields));
          access = await call(commands.connectionEditorAccess(editing));
        }
        if (access.kind !== "ready") { close(); return; }
        // The "Serialize" variant only differs in optional markers: same fields.
        const [saved, revision] = await call(commands.connectionDraft(editing));
        const draft = saved as ConnectionDraft;
        setRevision(revision);
        setInitial(draft);
        setEngine(catalog.find((e) => e.id === draft.engine) ?? null);
      } catch (e) {
        setLoadError(errorText(e));
      }
    });
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
            {editing
              ? t("editTitle", { name: initial?.name ?? editing })
              : importing
                ? t("dbeaver.title")
                : engine
                  ? t("formTitle", { engine: engine.name })
                  : t("title")}
          </h2>
          <button className="ghost" aria-label={t("close")} onClick={close}>
            ×
          </button>
        </div>
        {loadError ? (
          <p className="error wizard-error" role="alert">{loadError}</p>
        ) : editing ? (
          engine && initial && revision !== null && <ConnectionForm engine={engine} editing={editing} initial={initial} revision={revision} onCancel={close} onSaved={close} />
        ) : importing ? (
          <DbeaverImport onBack={() => setImporting(false)} onDone={close} />
        ) : engine ? (
          <ConnectionForm
            engine={engine}
            prefill={prefill ?? undefined}
            onBack={() => {
              setEngine(null);
              setPrefill(null);
            }}
            onCancel={close}
            onSaved={close}
          />
        ) : (
          <EnginePicker
            engines={engines}
            onImport={() => setImporting(true)}
            onPick={(picked, fields) => {
              setPrefill(fields ?? null);
              setEngine(picked);
            }}
          />
        )}
      </div>
    </div>
  );
}

function EngineBadge({ engine }: { engine: EngineItem }) {
  return (
    <EngineIcon engine={engine.id} />
  );
}

function EnginePicker({
  engines,
  onPick,
  onImport,
}: {
  engines: EngineItem[];
  onPick(engine: EngineItem, fields?: UrlFields): void;
  onImport(): void;
}) {
  const { t } = useTranslation("connections");
  const [search, setSearch] = useState("");
  const [url, setUrl] = useState("");
  const [urlError, setUrlError] = useState<string | null>(null);
  async function applyUrl() {
    setUrlError(null);
    try {
      const fields = await call(commands.parseConnectionUrl(url));
      const engine = engines.find((e) => e.id === fields.engine && e.available);
      if (engine) onPick(engine, fields);
      else setUrlError(t("url.unsupported"));
    } catch (e) {
      setUrlError(errorText(e));
    }
  }
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
      <form
        className="url-import"
        onSubmit={(e) => {
          e.preventDefault();
          void applyUrl();
        }}
      >
        <input
          type="text"
          placeholder="postgres://user@host:5432/database"
          aria-label={t("url.label")}
          value={url}
          onChange={(e) => {
            setUrl(e.target.value);
            setUrlError(null);
          }}
          spellCheck={false}
          autoComplete="off"
        />
        <button type="submit" disabled={!url.trim()}>
          {t("url.use")}
        </button>
      </form>
      {urlError && (
        <p className="error small" role="alert">
          {urlError}
        </p>
      )}
      <div className="import-row">
        <span className="muted small">{t("dbeaver.or")}</span>
        <button onClick={onImport}>{t("dbeaver.button")}</button>
      </div>
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
type AuthMethod = "password" | "env" | "none" | "other" | "token";
/** Short-lived tokens from a cloud provider's CLI (ADR-022): the core writes the command. */
type CloudToken = "awsRds" | "azureAd" | "googleCloudSql";
const CLOUD_TOKENS: CloudToken[] = ["awsRds", "azureAd", "googleCloudSql"];
/** Engines that log in with a client certificate or a cloud token. */
const PASSWORDLESS_ENGINES = ["postgres", "mysql", "mariadb"];
type FormTab = "main" | "ssl" | "ssh" | "kubernetes" | "options";

function Field({ label, children, className }: { label: string; children: React.ReactNode; className?: string }) {
  return (
    <label className={`field ${className ?? ""}`}>
      <span>{label}</span>
      {children}
    </label>
  );
}

function authFrom(mode: PasswordMode): AuthMethod {
  switch (mode) {
    case "keychain":
    case "prompt":
      return "password";
    case "env":
      return "env";
    case "other":
      return "other";
    case "none":
      return "none";
    case "awsRds":
    case "azureAd":
    case "googleCloudSql":
      return "token";
  }
}

function ConnectionForm(props: {
  engine: EngineItem;
  /** Identifier of the connection being edited; absent when adding one. */
  editing?: string;
  initial?: ConnectionDraft;
  revision?: string;
  /** Values read from a pasted connection URL. */
  prefill?: UrlFields;
  onBack?(): void;
  onCancel(): void;
  onSaved(): void;
}) {
  const { engine } = props;
  const { t } = useTranslation("connections");
  const workspace = useApp((s) => s.workspace);
  const hasWorkspace = Boolean(workspace?.workspace);
  const network = engine.access === "network";
  // A SQLite file may be on a server, opened there through an SSH tunnel (ADR-018).
  const remoteFile = engine.id === "sqlite";
  const tunnelsAllowed = network || remoteFile;
  const [tab, setTab] = useState<FormTab>("main");
  const [tunnels, setTunnels] = useState<TunnelItem[]>([]);
  const { initial, editing, prefill } = props;
  const [auth, setAuth] = useState<AuthMethod>(initial ? authFrom(initial.passwordMode) : network ? "password" : "none");
  const [cloudToken, setCloudToken] = useState<CloudToken>(
    initial && (CLOUD_TOKENS as string[]).includes(initial.passwordMode) ? (initial.passwordMode as CloudToken) : "awsRds",
  );
  const passwordless = PASSWORDLESS_ENGINES.includes(engine.id);
  const [savePassword, setSavePassword] = useState(initial ? initial.passwordMode !== "prompt" : true);
  const [useTunnel, setUseTunnel] = useState(Boolean(initial?.tunnel));
  // Kubernetes and an SSH tunnel exclude each other (the core rejects both).
  const [useKubernetes, setUseKubernetes] = useState(Boolean(initial?.kubernetes));
  const [kubernetes, setKubernetes] = useState(initial?.kubernetes ?? EMPTY_KUBERNETES);
  const [password, setPassword] = useState(prefill?.password ?? "");
  const [test, setTest] = useState<TestState>({ kind: "idle" });
  const [saving, setSaving] = useState(false);
  const [fileError, setFileError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Omit<ConnectionDraft, "passwordMode" | "tunnel" | "kubernetes">>(() => initial ?? {
    personal: !hasWorkspace,
    folder: "",
    name: prefill ? prefill.database.split(/[\\/]/).pop() || prefill.host : "",
    engine: engine.id,
    env: "",
    host: prefill?.host || (network ? "localhost" : ""),
    port: prefill?.port ?? null,
    database: prefill?.database ?? "",
    user: prefill?.user ?? "",
    passwordEnv: "",
    passwordOther: "",
    tlsMode: prefill?.tlsMode ?? "prefer",
    tlsCa: "",
    tlsCert: "",
    tlsKey: "",
    readOnly: false,
    mcpExposed: false,
    mcpWrites: false,
    sqlite3: "",
    localPort: null,
    // New connections keep idle sessions alive (ADR-028); 0 or empty turns it off.
    keepalive: network ? 300 : null,
  });
  const [tunnel, setTunnel] = useState(initial?.tunnel ?? "");

  const [tunnelEditor, setTunnelEditor] = useState<TunnelEditorMode | null>(null);
  async function loadTunnels(select?: string) {
    try {
      const list = await call(commands.listTunnels());
      setTunnels(list);
      setTunnel((current) => select ?? (list.some((item) => item.name === current) ? current : (list[0]?.name ?? "")));
    } catch {
      setTunnels([]);
    }
  }
  useEffect(() => {
    if (tunnelsAllowed) void loadTunnels();
  }, [network]);
  const selectedTunnel = tunnels.find((item) => item.name === tunnel);

  const changed = () => {
    setTest({ kind: "idle" });
    setSaveError(null);
  };
  const update = (patch: Partial<typeof draft>) => {
    setDraft((d) => ({ ...d, ...patch }));
    changed();
  };
  const setPort = useCallback((port: number) => {
    setDraft((d) => ({ ...d, port }));
    setTest({ kind: "idle" });
    setSaveError(null);
  }, []);

  // Agents write only on an exposed connection that is not read-only (ADR-019).
  const agentsWrite = draft.mcpWrites && draft.mcpExposed && !draft.readOnly;
  // What the core receives: the form's choices mapped to the configuration format.
  const passwordMode: PasswordMode =
    !network || auth === "none"
      ? "none"
      : auth === "env"
        ? "env"
        : auth === "other"
          ? "other"
          : auth === "token"
            ? cloudToken
            : savePassword
              ? "keychain"
              : "prompt";
  const request: ConnectionDraft = {
    ...draft,
    mcpWrites: agentsWrite,
    passwordMode,
    tunnel: tunnelsAllowed && useTunnel && !useKubernetes ? tunnel : "",
    sqlite3: remoteFile && useTunnel ? draft.sqlite3 : "",
    localPort: network && useTunnel && !useKubernetes ? (draft.localPort ?? null) : null,
    kubernetes: network && useKubernetes ? kubernetes : null,
  };
  const typedPassword = auth === "password" ? password : "";
  const busy = saving || test.kind === "testing";

  async function trustForKubernetes(): Promise<boolean> {
    if (useApp.getState().workspace?.trusted) return true;
    try {
      const shown = useApp.getState().workspace?.workspace ?? null;
      if (!(await useDialogs.getState().askTrust(shown))) return false;
      useApp.getState().setWorkspace(await call(commands.trustWorkspace(shown ?? "")));
      return true;
    } catch (e) {
      setSaveError(errorText(e));
      return false;
    }
  }

  async function browse(field: "database" | "tlsCa" | "tlsCert" | "tlsKey") {
    const path = await open({
      multiple: false,
      directory: false,
      filters: openFilters(
        field === "database"
          ? [...databaseFilters(), { name: t("allFiles"), extensions: ["*"] }]
          : [{ name: t("certificates"), extensions: ["pem", "crt", "cer", "key"] }, { name: t("allFiles"), extensions: ["*"] }],
      ),
    });
    if (typeof path === "string") update({ [field]: path });
  }

  function databaseFilters() {
    const files = DATABASE_FILES[engine.id];
    return files ? [{ name: files.name, extensions: files.extensions }] : [];
  }

  /** A new, empty database file created by the core (never over an existing one). */
  async function createDatabase() {
    setFileError(null);
    const path = await chooseSavePath({
      defaultPath: DATABASE_FILES[engine.id]?.fallback ?? "database.db",
      filters: databaseFilters(),
    });
    if (!path) return;
    try {
      await call(commands.createDatabase(engine.id, path));
      const stem = (path.split(/[\\/]/).pop() ?? "").replace(/\.[^.]*$/, "");
      update(draft.name.trim() ? { database: path } : { database: path, name: stem });
    } catch (e) {
      setFileError(e instanceof Error ? e.message : String(e));
    }
  }

  async function runTest() {
    setTest({ kind: "testing" });
    try {
      let outcome = await call(commands.testConnection(request, typedPassword || null, editing ?? null));
      if (outcome.kind === "untrusted") {
        if (!(await useDialogs.getState().askTrust(workspace?.workspace ?? null))) {
          setTest({ kind: "idle" });
          return;
        }
        useApp.getState().setWorkspace(await call(commands.trustWorkspace(workspace?.workspace ?? "")));
        outcome = await call(commands.testConnection(request, typedPassword || null, editing ?? null));
      }
      // The tunnel this connection uses may have changed in the repository.
      const reviewed = await reviewChanges(outcome, () =>
        call(commands.testConnection(request, typedPassword || null, editing ?? null)),
      );
      if (!reviewed) {
        setTest({ kind: "idle" });
        return;
      }
      outcome = reviewed;
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
        case "changed":
        case "missingVariables":
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
      const stored = passwordMode === "keychain" && password ? password : null;
      await call(editing ? commands.updateConnection(editing, request, stored, props.revision ?? "") : commands.addConnection(request, stored));
      await refreshConnections();
      props.onSaved();
    } catch (e) {
      setSaveError(errorText(e));
    } finally {
      setSaving(false);
    }
  }

  const onServer = remoteFile && useTunnel;
  const tabs: FormTab[] = network
    ? ["main", "ssl", "ssh", "kubernetes", "options"]
    : remoteFile
      ? ["main", "ssh", "options"]
      : ["main", "options"];
  return (
    <>
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
                {useKubernetes && <p className="muted small">{t("kubernetes.mainHint")}</p>}
              </fieldset>
              <fieldset>
                <legend>{t("sections.authentication")}</legend>
                <Field label={t("fields.authentication")}>
                  <select aria-label={t("fields.authentication")} value={auth} onChange={(e) => { setAuth(e.target.value as AuthMethod); changed(); }}>
                    <option value="password">{t("auth.password")}</option>
                    <option value="env">{t("auth.env")}</option>
                    <option value="none">{t("auth.none")}</option>
                    {passwordless && <option value="token">{t("auth.token")}</option>}
                    {initial?.passwordMode === "other" && <option value="other">{t("auth.other")}</option>}
                  </select>
                </Field>
                {auth === "token" && (
                  <>
                    <Field label={t("fields.cloudToken")}>
                      <select aria-label={t("fields.cloudToken")} value={cloudToken} onChange={(e) => { setCloudToken(e.target.value as CloudToken); changed(); }}>
                        {CLOUD_TOKENS.map((token) => (
                          <option key={token} value={token}>
                            {t(`cloudToken.${token}`)}
                          </option>
                        ))}
                      </select>
                    </Field>
                    <p className="muted small">{t(`cloudToken.${cloudToken}Hint`)}</p>
                  </>
                )}
                <Field label={t("fields.user")}>
                  <input type="text" value={draft.user} autoComplete="off" onChange={(e) => update({ user: e.target.value })} />
                </Field>
                {auth === "password" && (
                  <>
                    <Field label={t("fields.password")}>
                      <input
                        type="password"
                        value={password}
                        autoComplete="off"
                        placeholder={editing && initial?.passwordMode === "keychain" && savePassword ? t("auth.keepPlaceholder") : ""}
                        onChange={(e) => { setPassword(e.target.value); changed(); }}
                      />
                    </Field>
                    <label className="check">
                      <input type="checkbox" checked={savePassword} onChange={(e) => { setSavePassword(e.target.checked); changed(); }} />
                      {t("fields.savePassword")}
                    </label>
                    <p className="muted small">{savePassword ? t("auth.savedHint") : t("auth.askHint")}</p>
                  </>
                )}
                {auth === "other" && (
                  <>
                    <Field label={t("fields.passwordOther")}>
                      <input type="text" value={draft.passwordOther} readOnly />
                    </Field>
                    <p className="muted small">{t("auth.otherHint")}</p>
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
                <Field label={onServer ? t("fields.serverFile") : t("fields.file")} className="grow">
                  <input type="text" value={draft.database} placeholder={onServer ? "/srv/app/data.db" : ""} onChange={(e) => update({ database: e.target.value })} />
                </Field>
                {!onServer && (
                  <>
                    <button type="button" onClick={() => void browse("database")}>
                      {t("browse")}
                    </button>
                    <button type="button" onClick={() => void createDatabase()}>
                      {t("newDatabase")}
                    </button>
                  </>
                )}
              </div>
              {fileError && (
                <p className="error small" role="alert">
                  {fileError}
                </p>
              )}
              <p className="muted small">{onServer ? t("fields.serverFileHint") : t("fields.fileHint")}</p>
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
              {passwordless && (
                <>
                  <div className="form-pair">
                    <Field label={t("fields.tlsCert")} className="grow">
                      <input type="text" value={draft.tlsCert} placeholder={t("fields.tlsCertPlaceholder")} onChange={(e) => update({ tlsCert: e.target.value })} />
                    </Field>
                    <button type="button" onClick={() => void browse("tlsCert")}>
                      {t("browse")}
                    </button>
                  </div>
                  <div className="form-pair">
                    <Field label={t("fields.tlsKey")} className="grow">
                      <input type="text" value={draft.tlsKey} placeholder={t("fields.tlsKeyPlaceholder")} onChange={(e) => update({ tlsKey: e.target.value })} />
                    </Field>
                    <button type="button" onClick={() => void browse("tlsKey")}>
                      {t("browse")}
                    </button>
                  </div>
                  <p className="muted small">{t("fields.clientCertHint")}</p>
                </>
              )}
            </fieldset>
          </div>
        )}

        {tunnelsAllowed && (
          <div id="connection-tab-ssh" role="tabpanel" hidden={tab !== "ssh"}>
            <fieldset>
              <legend>{t("sections.ssh")}</legend>
              <label className="check">
                <input
                  type="checkbox"
                  checked={useTunnel && !useKubernetes}
                  disabled={tunnels.length === 0}
                  onChange={(e) => {
                    setUseTunnel(e.target.checked);
                    if (e.target.checked) setUseKubernetes(false);
                    changed();
                  }}
                />
                {remoteFile ? t("fields.useTunnelForFile") : t("fields.useTunnel")}
              </label>
              <div className="form-pair">
                <Field label={t("fields.tunnel")} className="grow">
                  <select aria-label={t("fields.tunnel")} value={tunnel} disabled={!useTunnel || useKubernetes} onChange={(e) => { setTunnel(e.target.value); changed(); }}>
                    {tunnels.map((item) => (
                      <option key={`${item.personal}:${item.name}`} value={item.name}>
                        {item.name}
                        {item.personal ? ` (${t("fields.personal")})` : ""}
                      </option>
                    ))}
                  </select>
                </Field>
                <button type="button" disabled={!useTunnel || useKubernetes || !selectedTunnel} onClick={() => selectedTunnel && setTunnelEditor({ kind: "edit", name: selectedTunnel.name, personal: selectedTunnel.personal })}>
                  {t("tunnel.editButton")}
                </button>
                <button type="button" onClick={() => setTunnelEditor({ kind: "add", personal: draft.personal })}>
                  {t("tunnel.newButton")}
                </button>
              </div>
              <p className="muted small">
                {tunnels.length === 0 ? t("fields.noTunnelsHint") : remoteFile ? t("fields.tunnelForFileHint") : t("fields.tunnelHint")}
              </p>
              {!remoteFile && (
                <>
                  <Field label={t("fields.localPort")} className="port">
                    <input
                      type="number"
                      min={1}
                      max={65535}
                      value={draft.localPort ?? ""}
                      disabled={!useTunnel || useKubernetes}
                      placeholder={t("fields.localPortPlaceholder")}
                      onChange={(e) => update({ localPort: e.target.value ? Math.trunc(Number(e.target.value)) : null })}
                    />
                  </Field>
                  <p className="muted small">{t("fields.localPortHint")}</p>
                </>
              )}
              {remoteFile && (
                <Field label={t("fields.sqlite3")}>
                  <input type="text" value={draft.sqlite3} disabled={!useTunnel} placeholder="sqlite3" onChange={(e) => update({ sqlite3: e.target.value })} />
                </Field>
              )}
            </fieldset>
          </div>
        )}

        {network && (
          <div id="connection-tab-kubernetes" role="tabpanel" hidden={tab !== "kubernetes"}>
            <KubernetesTab
              enabled={useKubernetes}
              personal={draft.personal}
              editing={editing ?? null}
              trusted={Boolean(workspace?.trusted)}
              onEnabled={(enabled) => {
                void (async () => {
                  if (enabled && !draft.personal && !(await trustForKubernetes())) return;
                  setUseKubernetes(enabled);
                  if (enabled) setUseTunnel(false);
                  changed();
                })();
              }}
              value={kubernetes}
              onChange={(next) => {
                setKubernetes(next);
                changed();
              }}
              port={draft.port ?? null}
              defaultPort={engine.defaultPort ?? null}
              onPort={setPort}
            />
          </div>
        )}

        <div id="connection-tab-options" role="tabpanel" hidden={tab !== "options"}>
          <fieldset>
            <legend>{t("sections.organization")}</legend>
            <div className="form-row" role="radiogroup" aria-label={t("fields.saveIn")}>
              <span>{t("fields.saveIn")}</span>
              <label className="check">
                <input type="radio" name="save-in" checked={!draft.personal} disabled={!hasWorkspace || Boolean(editing)} onChange={() => {
                  void (async () => {
                    if (useKubernetes && !(await trustForKubernetes())) return;
                    update({ personal: false });
                  })();
                }} />
                {t("fields.project")}
              </label>
              <label className="check">
                <input type="radio" name="save-in" checked={draft.personal} disabled={Boolean(editing)} onChange={() => update({ personal: true })} />
                {t("fields.personal")}
              </label>
            </div>
            <p className="muted small">
              {editing ? t("fields.fixedLocationHint") : draft.personal ? t("fields.personalHint") : t("fields.projectHint")}
            </p>
            <div className="form-pair">
              <Field label={t("fields.folder")} className="grow">
                <input type="text" value={draft.folder} disabled={Boolean(editing)} placeholder={t("fields.folderPlaceholder")} onChange={(e) => update({ folder: e.target.value })} />
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
          {network && (
            <fieldset>
              <legend>{t("sections.idle")}</legend>
              <Field label={t("fields.keepalive")} className="port">
                <input
                  type="number"
                  min={0}
                  max={86400}
                  value={draft.keepalive ?? ""}
                  placeholder={t("fields.keepaliveOff")}
                  onChange={(e) => update({ keepalive: e.target.value ? Math.max(0, Math.trunc(Number(e.target.value))) : null })}
                />
              </Field>
              <p className="muted small">{t("fields.keepaliveHint")}</p>
            </fieldset>
          )}
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
            <Field label={t("fields.mcpAccess")}>
              <select
                aria-label={t("fields.mcpAccess")}
                value={agentsWrite ? "write" : "read"}
                disabled={!draft.mcpExposed || draft.readOnly}
                onChange={(e) => update({ mcpWrites: e.target.value === "write" })}
              >
                <option value="read">{t("fields.mcpRead")}</option>
                <option value="write">{t("fields.mcpWrite")}</option>
              </select>
            </Field>
            {agentsWrite && draft.env.trim().toLowerCase() === "prod" && (
              <p className="banner" role="alert">{t("fields.mcpProdWarning")}</p>
            )}
            <p className="muted small">{agentsWrite ? t("fields.mcpWriteHint") : t("fields.mcpHint")}</p>
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
        {props.onBack && (
          <button type="button" onClick={props.onBack} disabled={busy}>
            {t("back")}
          </button>
        )}
        <button type="button" onClick={props.onCancel} disabled={saving}>
          {t("cancel")}
        </button>
        <button type="submit" className="primary" disabled={busy}>
          {saving ? t("saving") : editing ? t("saveChanges") : t("save")}
        </button>
      </div>
    </form>
    {tunnelEditor && (
      <TunnelEditor
        mode={tunnelEditor}
        onClose={() => setTunnelEditor(null)}
        onSaved={(saved) => {
          setTunnelEditor(null);
          setUseTunnel(true);
          setUseKubernetes(false);
          changed();
          void loadTunnels(saved.name);
        }}
        onDeleted={() => {
          setTunnelEditor(null);
          setUseTunnel(false);
          changed();
          void loadTunnels();
        }}
      />
    )}
    </>
  );
}
