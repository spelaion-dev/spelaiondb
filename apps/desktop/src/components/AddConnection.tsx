// "Add connection" wizard: 1) pick a database from the catalog, grouped by category;
// 2) fill in the engine's form, test it and save it. Validation and writing the file live
// in the core: this only collects what the user typed.

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  call,
  commands,
  type ConnectionDraft,
  type EngineChangeItem,
  type EngineItem,
  type PasswordMode,
  type TlsModeItem,
  type TunnelItem,
  type UrlFields,
} from "../api";
import { tagsInUse } from "../connectionInfo";
import { type ConnectionDialog, useApp, useDialogs } from "../store";
import { ConnectionTypePicker, pickedType, useConnectionTypes } from "./ConnectionTypePicker";
import { EngineIcon } from "./EngineIcon";
import { TagInput } from "./TagInput";
import { refreshConnections } from "./Sidebar";
import { chooseOpenFile, chooseSavePath } from "../dialogs";
import { confirmOpenTransactions, reviewChanges } from "../connect";
import { useKeychainAvailable } from "../keychain";
import { ensureEditorAccess } from "../connectionCopies";
import { saveHistoryPreference } from "../historyPreference";
import { fieldList } from "../fieldNames";
import { ConnectionImport, IMPORT_CLIENTS, type ImportClientId } from "./ConnectionImport";
import { TunnelEditor, type TunnelEditorMode } from "./TunnelEditor";
import { EMPTY_KUBERNETES, KubernetesTab } from "./KubernetesTab";
import { ConnectionTestLog, TestSummary, useConnectionTest } from "./ConnectionTestLog";

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
  const [importing, setImporting] = useState<ImportClientId | null>(null);
  const [initial, setInitial] = useState<ConnectionDraft | null>(null);
  const [revision, setRevision] = useState<string | null>(null);
  const [loadError, setLoadError] = useState<string | null>(null);
  /** The engine the edited connection has on disk, and what switching it did (ADR-030). */
  const [savedEngine, setSavedEngine] = useState<string | null>(null);
  const [engineChange, setEngineChange] = useState<EngineChangeItem | null>(null);
  const [carriedPassword, setCarriedPassword] = useState("");

  /** The core maps the form to another engine; the form starts again with the result. */
  async function changeEngine(next: EngineItem, current: ConnectionDraft, password: string) {
    const mapped = await call(commands.changeConnectionEngine(current, next.id));
    setInitial(mapped.draft as ConnectionDraft);
    setEngine(next);
    setCarriedPassword(password);
    setEngineChange(next.id === savedEngine ? null : mapped.change);
  }

  useEffect(() => {
    void commands.engineCatalog().then(async (catalog) => {
      setEngines(catalog);
      if (!editing) return;
      try {
        if (!(await ensureEditorAccess(editing))) { close(); return; }
        // The "Serialize" variant only differs in optional markers: same fields.
        const [saved, revision] = await call(commands.connectionDraft(editing));
        const draft = saved as ConnectionDraft;
        setRevision(revision);
        setInitial(draft);
        setSavedEngine(draft.engine);
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
                ? t("importer.title", { client: IMPORT_CLIENTS.find((c) => c.id === importing)?.label })
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
          engine &&
          initial &&
          revision !== null && (
            <>
              {dialog.kind === "edit" && dialog.notice && (
                <p className="banner wizard-notice" role="status">
                  {dialog.notice}
                </p>
              )}
              <ConnectionForm
                // Another engine is another form, started from what the core mapped.
                key={engine.id}
                engine={engine}
                editing={editing}
                initial={initial}
                initialPassword={carriedPassword}
                revision={revision}
                engines={engines.filter((e) => e.available)}
                savedEngine={savedEngine ?? engine.id}
                engineChange={engineChange}
                onChangeEngine={changeEngine}
                onCancel={close}
                onSaved={close}
              />
            </>
          )
        ) : importing ? (
          <ConnectionImport client={importing} onBack={() => setImporting(null)} onDone={close} />
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
            onImport={(client) => setImporting(client)}
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
  onImport(client: ImportClientId): void;
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
        <span className="muted small">{t("importer.or")}</span>
        {IMPORT_CLIENTS.map((client) => (
          <button key={client.id} onClick={() => onImport(client.id)}>
            {t("importer.button", { client: client.label })}
          </button>
        ))}
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

/** How the form asks for credentials, like DBeaver's "Authentication" selector. */
type AuthMethod = "password" | "env" | "none" | "other" | "token";
/** Short-lived tokens from a cloud provider's CLI (ADR-022): the core writes the command. */
type CloudToken = "awsRds" | "azureAd" | "googleCloudSql";
const CLOUD_TOKENS: CloudToken[] = ["awsRds", "azureAd", "googleCloudSql"];
/** Engines that log in with a client certificate or a cloud token. */
const PASSWORDLESS_ENGINES = ["postgres", "mysql", "mariadb"];
type FormTab = "main" | "ssl" | "ssh" | "kubernetes" | "session" | "options";

/** The core's limit (`ConnectionFile::MAX_DESCRIPTION`); the core checks it again. */
const DESCRIPTION_MAX = 2000;
/** The core's limits (`SessionConfig::MAX_SETUP`, `MAX_SCHEMA`); the core checks them again. */
const SETUP_MAX = 20000;
const SCHEMA_MAX = 128;
/** An example of each engine's session setup, shown while the field is empty (SQL, not prose). */
const SETUP_EXAMPLES: Record<string, string> = {
  postgres: "SET search_path TO app, public;\nSET TIME ZONE 'UTC';",
  mysql: "SET time_zone = '+00:00';",
  mariadb: "SET time_zone = '+00:00';",
  sqlserver: "SET DATEFORMAT ymd;\nSET LOCK_TIMEOUT 5000;",
  duckdb: "SET TimeZone = 'UTC';",
  sqlite: "PRAGMA foreign_keys = ON;",
};

function Field({ label, children, className }: { label: string; children: React.ReactNode; className?: string }) {
  return (
    <label className={`field ${className ?? ""}`}>
      <span>{label}</span>
      {children}
    </label>
  );
}

/** What switching the engine kept, changed and dropped, shown before saving (ADR-030). */
function EngineChangeSummary({ change, engines }: { change: EngineChangeItem; engines: EngineItem[] }) {
  const { t } = useTranslation("connections");
  const engineName = (id: string) => engines.find((e) => e.id === id)?.name ?? id;
  const shown = (value: string | null) => value ?? t("engineChange.none");
  return (
    <div className="banner engine-change" role="status" aria-label={t("engineChange.title")}>
      <strong>{t("engineChange.summary", { from: engineName(change.from), to: engineName(change.to) })}</strong>
      {change.kept.length > 0 && <p>{t("engineChange.kept", { fields: fieldList(change.kept) })}</p>}
      {change.changed.length > 0 && (
        <p>
          {t("engineChange.changed", {
            fields: change.changed.map((c) => `${fieldList([c.field])}: ${shown(c.before)} → ${shown(c.after)}`).join("; "),
          })}
        </p>
      )}
      {change.dropped.length > 0 && <p>{t("engineChange.dropped", { fields: fieldList(change.dropped) })}</p>}
      {change.required.length > 0 && <p className="error">{t("engineChange.required", { fields: fieldList(change.required) })}</p>}
      <p className="muted small">{t("engineChange.hint")}</p>
    </div>
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
  /** A password typed before the engine was switched, kept in the new form. */
  initialPassword?: string;
  /** Editing: the engines it can switch to, the one on disk, and what the last switch did. */
  engines?: EngineItem[];
  savedEngine?: string;
  engineChange?: EngineChangeItem | null;
  onChangeEngine?(engine: EngineItem, current: ConnectionDraft, password: string): Promise<void>;
  onBack?(): void;
  onCancel(): void;
  onSaved(): void;
}) {
  const { engine } = props;
  const { t } = useTranslation("connections");
  const workspace = useApp((s) => s.workspace);
  const hasWorkspace = Boolean(workspace?.workspace);
  /** Tags other connections already use, offered while typing a new one. */
  const connections = useApp((s) => s.connections);
  const knownTags = tagsInUse(connections);
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
  // Without a system keychain a new connection asks for its password; an edited one keeps
  // what its file says (a repository file is the team's) unless the user unticks it.
  const keychain = useKeychainAvailable();
  useEffect(() => {
    if (keychain === false && !initial) setSavePassword(false);
  }, [keychain, initial]);
  const [useTunnel, setUseTunnel] = useState(Boolean(initial?.tunnel));
  // Kubernetes and an SSH tunnel exclude each other (the core rejects both).
  const [useKubernetes, setUseKubernetes] = useState(Boolean(initial?.kubernetes));
  const [kubernetes, setKubernetes] = useState(initial?.kubernetes ?? EMPTY_KUBERNETES);
  const [password, setPassword] = useState(prefill?.password ?? props.initialPassword ?? "");
  /** The user typed a name: the pattern stops filling it (until the field is emptied). */
  const [nameTyped, setNameTyped] = useState(Boolean(editing));
  const [engineError, setEngineError] = useState<string | null>(null);
  // The step-by-step test (ADR-032); `testError` is a failure before any step could run.
  const test = useConnectionTest();
  const [testError, setTestError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [fileError, setFileError] = useState<string | null>(null);
  const [saveError, setSaveError] = useState<string | null>(null);
  const [draft, setDraft] = useState<Omit<ConnectionDraft, "passwordMode" | "tunnel" | "kubernetes">>(() => initial ?? {
    personal: !hasWorkspace,
    folder: "",
    name: prefill ? prefill.database.split(/[\\/]/).pop() || prefill.host : "",
    description: "",
    tags: [],
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
    connectTimeout: null,
    statementTimeout: null,
    sessionSetup: "",
    ignoreSetupErrors: false,
    defaultSchema: "",
    isolation: null,
  });
  const [tunnel, setTunnel] = useState(initial?.tunnel ?? "");
  // Types and what they do come from the core (ADR-029).
  const connectionTypes = useConnectionTypes();
  const editedConnection = useApp((s) => s.connections.find((c) => c.id === editing));
  const savedType = editing && initial ? { env: initial.env, kind: editedConnection?.kind ?? null } : null;
  // Personal, in the local store and never in the file: saved with the form, apart from it.
  const savedHistory = editedConnection?.historyEnabled ?? true;
  const [historyEnabled, setHistoryEnabled] = useState(savedHistory);

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
    test.clear();
    setTestError(null);
    setSaveError(null);
  };
  const update = (patch: Partial<typeof draft>) => {
    setDraft((d) => ({ ...d, ...patch }));
    changed();
  };
  const setPort = useCallback((port: number) => {
    setDraft((d) => ({ ...d, port }));
    test.clear();
    setTestError(null);
    setSaveError(null);
  }, []);

  // Agents write only on an exposed connection that is not read-only (ADR-019).
  const agentsWrite = draft.mcpWrites && draft.mcpExposed && !draft.readOnly;
  const selectedType = pickedType(connectionTypes, draft.env, savedType);
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
  const busy = saving || test.running;

  // A new connection is named by the personal pattern (ADR-030) until the user types a name.
  const suggestKey = JSON.stringify([request.engine, request.host, request.port, request.database, request.user, request.env]);
  useEffect(() => {
    if (nameTyped) return;
    let current = true;
    call(commands.suggestConnectionName(request))
      .then((name) => {
        if (current && name !== null) setDraft((d) => ({ ...d, name }));
      })
      .catch(() => undefined);
    return () => {
      current = false;
    };
  }, [suggestKey, nameTyped]);

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
    const path = await chooseOpenFile(
      undefined,
      field === "database"
        ? [...databaseFilters(), { name: t("allFiles"), extensions: ["*"] }]
        : [{ name: t("certificates"), extensions: ["pem", "crt", "cer", "key"] }, { name: t("allFiles"), extensions: ["*"] }],
      draft[field],
    );
    if (path) update({ [field]: path });
  }

  function databaseFilters() {
    const files = DATABASE_FILES[engine.id];
    return files ? [{ name: files.name, extensions: files.extensions }] : [];
  }

  /** A new, empty database file created by the core (never over an existing one). */
  async function createDatabase() {
    setFileError(null);
    try {
      // Inside the try: a dialog that fails (a missing permission, no portal on Linux)
      // must show its error, not leave the button doing nothing.
      const path = await chooseSavePath({
        defaultPath: DATABASE_FILES[engine.id]?.fallback ?? "database.db",
        filters: databaseFilters(),
      });
      if (!path) return;
      await call(commands.createDatabase(engine.id, path));
      const stem = (path.split(/[\\/]/).pop() ?? "").replace(/\.[^.]*$/, "");
      update(draft.name.trim() ? { database: path } : { database: path, name: stem });
    } catch (e) {
      setFileError(e instanceof Error ? e.message : String(e));
    }
  }

  /** The step-by-step test (ADR-032): the core runs and classifies each step and builds the
   *  diagnostics; this answers the questions that come before it and shows the log. */
  async function runTest(networkOnly: boolean) {
    setTestError(null);
    try {
      const attempt = () =>
        test.run((id, onStep) => call(commands.testConnection(request, typedPassword || null, editing ?? null, networkOnly, id, onStep)));
      let outcome = await attempt();
      if (outcome.kind === "untrusted") {
        if (!(await useDialogs.getState().askTrust(workspace?.workspace ?? null))) {
          test.clear();
          return;
        }
        useApp.getState().setWorkspace(await call(commands.trustWorkspace(workspace?.workspace ?? "")));
        outcome = await attempt();
      }
      // The tunnel this connection uses may have changed in the repository.
      const reviewed = await reviewChanges(outcome, attempt);
      if (!reviewed || reviewed.kind !== "tested") {
        test.clear();
        return;
      }
      test.finish(reviewed.report);
    } catch (e) {
      test.clear();
      setTestError(errorText(e));
    }
  }

  async function save() {
    setSaving(true);
    setSaveError(null);
    try {
      // Only a saved password reaches the keychain; otherwise it was just for the test.
      const stored = passwordMode === "keychain" && password ? password : null;
      // Changing the engine closes its tabs' sessions: ask first if a transaction is open.
      if (editing && props.savedEngine && props.savedEngine !== engine.id) {
        const sessions = useApp.getState().tabs.filter((tab) => tab.connectionId === editing).map((tab) => tab.id);
        if (!(await confirmOpenTransactions(sessions, "disconnect"))) return;
      }
      if (editing) {
        // The local setting first: if it cannot change, nothing was written yet.
        if (historyEnabled !== savedHistory) await saveHistoryPreference(editing, historyEnabled);
        await call(commands.updateConnection(editing, request, stored, props.revision ?? ""));
      } else {
        const id = await call(commands.addConnection(request, stored));
        await refreshConnections();
        // The store remembers it per connection identifier (enabled unless set otherwise).
        const added = useApp.getState().connections.find((c) => c.id === id);
        if (added && added.historyEnabled !== historyEnabled) {
          try {
            await saveHistoryPreference(id, historyEnabled);
          } catch (e) {
            // The connection exists: saving this form again would add a second one.
            useApp.getState().setConnectionDialog({ kind: "edit", id, notice: t("fields.historyNotSaved", { error: errorText(e) }) });
            return;
          }
        }
      }
      if (editing && props.savedEngine && props.savedEngine !== engine.id) {
        // Its open sessions belong to the old engine: closed; tabs connect again when run.
        const app = useApp.getState();
        const tabs = app.tabs.filter((tab) => tab.connectionId === editing);
        await Promise.all(
          [...tabs.map((tab) => tab.id), `meta:${editing}`].map((session) => call(commands.disconnect(session)).catch(() => undefined)),
        );
        for (const tab of tabs) app.updateTab(tab.id, { status: "disconnected", serverVersion: null });
      }
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
    ? ["main", "ssl", "ssh", "kubernetes", "session", "options"]
    : remoteFile
      ? ["main", "ssh", "session", "options"]
      : ["main", "session", "options"];
  // Where the session setup may only read and change settings (ADR-037), as the core decides
  // when the session opens: a read-only connection or a type that confirms data changes.
  const setupRestricted = draft.readOnly || Boolean(selectedType?.safeguards.confirmDataChanges);
  const databaseIsSchema = engine.id === "mysql" || engine.id === "mariadb";
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
          <input
            type="text"
            autoFocus
            value={draft.name}
            placeholder={t("fields.namePlaceholder", { engine: engine.name })}
            onChange={(e) => {
              update({ name: e.target.value });
              setNameTyped(e.target.value.trim() !== "");
            }}
          />
        </Field>
        {props.engines && props.onChangeEngine && (
          <Field label={t("fields.engine")}>
            <select
              aria-label={t("fields.engine")}
              value={engine.id}
              disabled={busy}
              onChange={(e) => {
                const next = props.engines?.find((item) => item.id === e.target.value);
                if (!next || !props.onChangeEngine) return;
                setEngineError(null);
                props.onChangeEngine(next, request, password).catch((err: unknown) => setEngineError(errorText(err)));
              }}
            >
              {props.engines.map((item) => (
                <option key={item.id} value={item.id}>
                  {item.name}
                </option>
              ))}
            </select>
          </Field>
        )}
      </div>
      {props.engineChange && <EngineChangeSummary change={props.engineChange} engines={props.engines ?? []} />}
      {engineError && (
        <p className="error small" role="alert">
          {engineError}
        </p>
      )}
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
                      <input
                        type="checkbox"
                        checked={savePassword}
                        disabled={keychain === false && !savePassword}
                        onChange={(e) => { setSavePassword(e.target.checked); changed(); }}
                      />
                      {t("fields.savePassword")}
                    </label>
                    <p className="muted small">{keychain === false ? t("auth.noKeychainHint") : savePassword ? t("auth.savedHint") : t("auth.askHint")}</p>
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

        <div id="connection-tab-session" role="tabpanel" hidden={tab !== "session"}>
          {(engine.sessionSchema || engine.isolationLevels.length > 0) && (
            <fieldset>
              <legend>{t("sections.session")}</legend>
              {engine.sessionSchema && (
                <>
                  <Field label={databaseIsSchema ? t("fields.defaultDatabase") : t("fields.defaultSchema")}>
                    <input
                      type="text"
                      name="defaultSchema"
                      maxLength={SCHEMA_MAX}
                      value={draft.defaultSchema}
                      placeholder={t("fields.defaultSchemaPlaceholder")}
                      onChange={(e) => update({ defaultSchema: e.target.value })}
                    />
                  </Field>
                  <p className="muted small">{databaseIsSchema ? t("fields.defaultDatabaseHint") : t("fields.defaultSchemaHint")}</p>
                </>
              )}
              {engine.isolationLevels.length > 0 && (
                <>
                  <Field label={t("fields.isolation")}>
                    <select aria-label={t("fields.isolation")} name="isolation" value={draft.isolation ?? ""} onChange={(e) => update({ isolation: e.target.value || null })}>
                      <option value="">{t("isolation.default")}</option>
                      {engine.isolationLevels.map((level) => (
                        <option key={level} value={level}>
                          {t(`isolation.${level}`)}
                        </option>
                      ))}
                    </select>
                  </Field>
                  <p className="muted small">{t("fields.isolationHint")}</p>
                </>
              )}
            </fieldset>
          )}
          <fieldset>
            <legend>{t("sections.setup")}</legend>
            <Field label={t("fields.sessionSetup")}>
              <textarea
                className="session-setup"
                name="sessionSetup"
                rows={6}
                spellCheck={false}
                maxLength={SETUP_MAX}
                value={draft.sessionSetup}
                placeholder={SETUP_EXAMPLES[engine.id] ?? ""}
                onChange={(e) => update({ sessionSetup: e.target.value })}
              />
            </Field>
            <label className="check">
              <input
                type="checkbox"
                name="ignoreSetupErrors"
                checked={draft.ignoreSetupErrors && draft.sessionSetup.trim() !== ""}
                disabled={draft.sessionSetup.trim() === ""}
                onChange={(e) => update({ ignoreSetupErrors: e.target.checked })}
              />
              {t("fields.ignoreSetupErrors")}
            </label>
            <p className="muted small">{t("fields.sessionSetupHint")}</p>
            {setupRestricted && <p className="muted small">{t("fields.sessionSetupRestricted")}</p>}
          </fieldset>
        </div>

        <div id="connection-tab-options" role="tabpanel" hidden={tab !== "options"}>
          <fieldset>
            <legend>{t("sections.about")}</legend>
            {/* What kind of connection it is, what it is for, and labels to find it. */}
            <Field label={t("fields.env")}>
              <ConnectionTypePicker types={connectionTypes} env={draft.env} saved={savedType} onChange={(env) => update({ env })} />
            </Field>
            <Field label={t("fields.description")}>
              <textarea
                className="description"
                rows={3}
                maxLength={DESCRIPTION_MAX}
                value={draft.description}
                placeholder={t("fields.descriptionPlaceholder")}
                onChange={(e) => update({ description: e.target.value })}
              />
            </Field>
            <p className="muted small">{t("fields.descriptionHint")}</p>
            <div className="field">
              <span>{t("fields.tags")}</span>
              <TagInput
                label={t("fields.tags")}
                tags={draft.tags}
                suggestions={knownTags}
                onChange={(tags) => update({ tags })}
              />
            </div>
            <p className="muted small">{t("fields.tagsHint")}</p>
          </fieldset>
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
            <Field label={t("fields.folder")}>
              <input type="text" value={draft.folder} disabled={Boolean(editing)} placeholder={t("fields.folderPlaceholder")} onChange={(e) => update({ folder: e.target.value })} />
            </Field>
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
              <Field label={t("fields.connectTimeout")} className="port">
                <input
                  type="number"
                  min={1}
                  max={600}
                  value={draft.connectTimeout ?? ""}
                  placeholder="15"
                  onChange={(e) => update({ connectTimeout: e.target.value ? Math.max(1, Math.trunc(Number(e.target.value))) : null })}
                />
              </Field>
              <p className="muted small">{t("fields.connectTimeoutHint")}</p>
            </fieldset>
          )}
          <fieldset>
            <legend>{t("sections.statements")}</legend>
            <Field label={t("fields.statementTimeout")} className="port">
              <input
                type="number"
                name="statementTimeout"
                min={0}
                max={86400}
                value={draft.statementTimeout ?? ""}
                placeholder={t("fields.statementTimeoutNone")}
                onChange={(e) => update({ statementTimeout: e.target.value ? Math.max(0, Math.trunc(Number(e.target.value))) : null })}
              />
            </Field>
            <p className="muted small">{t("fields.statementTimeoutHint")}</p>
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
            {agentsWrite && selectedType?.safeguards.confirmDataChanges && (
              <p className="banner" role="alert">{t("fields.mcpProdWarning")}</p>
            )}
            <p className="muted small">{agentsWrite ? t("fields.mcpWriteHint") : t("fields.mcpHint")}</p>
          </fieldset>
          <fieldset>
            <legend>{t("sections.onThisComputer")}</legend>
            <label className="check">
              <input type="checkbox" name="historyEnabled" checked={historyEnabled} onChange={(e) => { setHistoryEnabled(e.target.checked); setSaveError(null); }} />
              {t("fields.history")}
            </label>
            <p className="muted small">{t("fields.historyHint")}</p>
          </fieldset>
        </div>
      </div>

      {test.steps && (
        <ConnectionTestLog steps={test.steps} report={test.report} running={test.running} onStop={test.stop} onClose={test.clear} />
      )}
      <div className="wizard-footer">
        <button type="button" onClick={() => void runTest(false)} disabled={busy}>
          {t("test.button")}
        </button>
        {(network || onServer) && (
          <button type="button" onClick={() => void runTest(true)} disabled={busy}>
            {t("test.network")}
          </button>
        )}
        <div className="test-result" role="status">
          <TestSummary
            steps={test.steps}
            report={test.report}
            running={test.running}
            okText={(report) => (report.networkOnly ? t("testLog.okNetwork") : t("test.ok", { version: report.serverVersion ?? "" }))}
          />
          {testError && <span className="error">{testError}</span>}
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
