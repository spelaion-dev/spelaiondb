// The MCP server page (ADR-033): installing `spelaiondb mcp` in AI clients, and what agents may
// do. A thin adapter: the core finds the files, computes the change, writes it, and decides
// every permission; this page only shows them and asks.

import { useCallback, useEffect, useState } from "react";
import { useTranslation } from "react-i18next";
import {
  call,
  commands,
  type McpAccessItem,
  type McpClientItem,
  type McpClientsItem,
  type McpInstallPlanItem,
  type McpSettingsItem,
  type ToolOutputItem,
} from "../api";
import { useApp } from "../store";
import { TypeBadge } from "./TypeBadge";

type McpTab = "configuration" | "security";
const TABS: McpTab[] = ["configuration", "security"];

function message(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

export function McpPage() {
  const { t } = useTranslation("mcp");
  const [tab, setTab] = useState<McpTab>("configuration");
  return (
    <section className="mcp-page" aria-labelledby="mcp-title">
      <header className="mcp-header">
        <h1 id="mcp-title">{t("title")}</h1>
        <p>{t("intro")}</p>
      </header>
      <div className="mcp-tabs" role="tablist" aria-label={t("sections")}>
        {TABS.map((name) => (
          <button
            key={name}
            id={`mcp-tab-${name}`}
            role="tab"
            aria-selected={tab === name}
            aria-controls={`mcp-panel-${name}`}
            tabIndex={tab === name ? 0 : -1}
            onClick={() => setTab(name)}
            onKeyDown={(e) => {
              const index = TABS.indexOf(name);
              const next = e.key === "ArrowRight" ? TABS[(index + 1) % TABS.length] : e.key === "ArrowLeft" ? TABS[(index + TABS.length - 1) % TABS.length] : undefined;
              if (next) setTab(next);
            }}
          >
            {t(`tabs.${name}`)}
          </button>
        ))}
      </div>
      <div className="mcp-panel" role="tabpanel" id={`mcp-panel-${tab}`} aria-labelledby={`mcp-tab-${tab}`}>
        {tab === "configuration" && <ConfigurationTab />}
        {tab === "security" && <SecurityTab />}
      </div>
    </section>
  );
}

// --- Configuration ----------------------------------------------------------------------

function ConfigurationTab() {
  const { t } = useTranslation("mcp");
  const [clients, setClients] = useState<McpClientsItem | null>(null);
  const [selected, setSelected] = useState("claude-desktop");
  const [error, setError] = useState<string | null>(null);
  const [plan, setPlan] = useState<McpInstallPlanItem | null>(null);
  const [notice, setNotice] = useState<string | null>(null);

  const load = useCallback(() => {
    setError(null);
    call(commands.mcpClients()).then(setClients, (e: unknown) => setError(message(e)));
  }, []);
  useEffect(load, [load]);

  async function review(client: McpClientItem) {
    setNotice(null);
    setError(null);
    try {
      setPlan(await call(commands.mcpPlanInstall(client.id)));
    } catch (e) {
      setError(message(e));
    }
  }

  const current = clients?.clients.find((c) => c.id === selected) ?? clients?.clients[0] ?? null;
  return (
    <div className="mcp-configuration">
      <div className="mcp-clients">
        {clients && !clients.cli && <p className="banner" role="alert">{t("config.noCli")}</p>}
        {clients?.cli && (
          <p className="muted mcp-cli">
            {t(`config.cli.${clients.cli.source}`)} <code>{clients.cli.path}</code>
          </p>
        )}
        {error && <p className="error" role="alert">{error}</p>}
        {notice && <p className="mcp-notice" role="status">{notice}</p>}
        <ul className="mcp-client-list" aria-label={t("config.clients")}>
          {clients?.clients.map((client) => (
            <li key={client.id}>
              <div
                className={`mcp-client-card${client.id === current?.id ? " selected" : ""}`}
                role="group"
                aria-label={client.name}
                onClick={() => setSelected(client.id)}
              >
                <div className="mcp-client-head">
                  <button className="link mcp-client-name" aria-pressed={client.id === current?.id} onClick={() => setSelected(client.id)}>
                    {client.name}
                  </button>
                  <span className={`badge mcp-state mcp-state-${client.state}`}>{t(`config.state.${client.state}`)}</span>
                </div>
                <code className="mcp-client-path" title={client.path}>{client.path}</code>
                {client.problem && <p className="error mcp-client-problem">{client.problem}</p>}
                <button
                  className={client.state === "installed" ? "" : "primary"}
                  disabled={!clients.cli || client.state === "installed" || client.state === "unreadable"}
                  onClick={(e) => {
                    e.stopPropagation();
                    setSelected(client.id);
                    void review(client);
                  }}
                >
                  {client.state === "different" ? t("config.update") : t("config.install")}
                </button>
              </div>
            </li>
          ))}
        </ul>
      </div>
      {current && <ManualConfig client={current} />}
      {plan && (
        <InstallDialog
          plan={plan}
          name={clients?.clients.find((c) => c.id === plan.client)?.name ?? plan.client}
          onClose={() => setPlan(null)}
          onDone={(text) => {
            setPlan(null);
            setNotice(text);
            load();
          }}
        />
      )}
    </div>
  );
}

function CopyButton({ text, label }: { text: string; label: string }) {
  const { t } = useTranslation("mcp");
  const [copied, setCopied] = useState(false);
  return (
    <button
      onClick={() => {
        void navigator.clipboard.writeText(text).then(() => {
          setCopied(true);
          window.setTimeout(() => setCopied(false), 1500);
        });
      }}
      aria-label={label}
    >
      {copied ? t("config.copied") : t("config.copy")}
    </button>
  );
}

function ManualConfig({ client }: { client: McpClientItem }) {
  const { t } = useTranslation("mcp");
  return (
    <aside className="mcp-manual" aria-label={t("config.manualFor", { client: client.name })}>
      <h2>{t("config.manualFor", { client: client.name })}</h2>
      <p className="muted">{t(client.manualLanguage === "toml" ? "config.manualToml" : "config.manualJson", { path: client.path })}</p>
      <div className="mcp-code">
        <pre aria-label={t("config.manualConfig")}>{client.manualConfig}</pre>
        <CopyButton text={client.manualConfig} label={t("config.copyConfig")} />
      </div>
      {client.addCommand && (
        <>
          <p className="muted">{t("config.orCommand")}</p>
          <div className="mcp-code">
            <pre aria-label={t("config.addCommand")}>{client.addCommand}</pre>
            <CopyButton text={client.addCommand} label={t("config.copyCommand")} />
          </div>
        </>
      )}
    </aside>
  );
}

function InstallDialog({ plan, name, onClose, onDone }: { plan: McpInstallPlanItem; name: string; onClose(): void; onDone(notice: string): void }) {
  const { t } = useTranslation("mcp");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [busy, onClose]);

  async function apply() {
    setBusy(true);
    setError(null);
    try {
      const done = await call(commands.mcpApplyInstall(plan.client, plan.fingerprint));
      onDone(done.backup ? t("install.doneBackup", { client: name, backup: done.backup }) : t("install.done", { client: name }));
    } catch (e) {
      setError(message(e));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div className="modal mcp-install-dialog" role="dialog" aria-modal="true" aria-labelledby="mcp-install-title">
        <h2 id="mcp-install-title">{t("install.title", { client: name })}</h2>
        <p>{t(plan.creates ? "install.creates" : "install.changes")} <code>{plan.path}</code></p>
        {!plan.creates && <p className="muted">{t("install.backup")}</p>}
        <pre className="mcp-diff" aria-label={t("install.diff")}>
          {plan.diff.map((line, i) => (
            <span key={i} className={`diff-${line.kind}`}>
              {line.kind === "gap" ? t("install.gap", { count: line.skipped }) : `${line.kind === "added" ? "+" : line.kind === "removed" ? "-" : " "} ${line.text}`}
              {"\n"}
            </span>
          ))}
        </pre>
        <p className="muted">{t("install.restart", { client: name })}</p>
        {error && <p className="error" role="alert">{error}</p>}
        <div className="modal-actions">
          <button onClick={onClose} disabled={busy}>{t("install.cancel")}</button>
          <button className="primary" onClick={() => void apply()} disabled={busy}>{t("install.write")}</button>
        </div>
      </div>
    </div>
  );
}

// --- Security ---------------------------------------------------------------------------

function SecurityTab() {
  const { t } = useTranslation("mcp");
  const [settings, setSettings] = useState<McpSettingsItem | null>(null);
  const [access, setAccess] = useState<McpAccessItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const connectionsRevision = useApp((s) => s.connections);

  const load = useCallback(() => {
    call(commands.mcpSettings()).then(setSettings, (e: unknown) => setError(message(e)));
    call(commands.mcpAccess()).then(setAccess, (e: unknown) => setError(message(e)));
  }, []);
  useEffect(load, [load, connectionsRevision]);

  /** Shows the new value at once, then saves it and reads back what the core kept. */
  async function update(shown: Partial<McpSettingsItem>, change: () => Promise<unknown>) {
    setError(null);
    setSettings((current) => (current ? { ...current, ...shown } : current));
    try {
      await change();
    } catch (e) {
      setError(message(e));
    }
    load();
  }

  return (
    <div className="mcp-security">
      {error && <p className="error" role="alert">{error}</p>}
      <section aria-labelledby="mcp-output-title">
        <h2 id="mcp-output-title">{t("security.output.title")}</h2>
        <p className="muted">{t("security.output.intro")}</p>
        <fieldset className="settings-choice" disabled={!settings} aria-labelledby="mcp-output-title">
          {(["json", "toon"] as ToolOutputItem[]).map((format) => (
            <label key={format}>
              <input
                type="radio"
                name="mcp-output"
                checked={settings?.outputFormat === format}
                onChange={() => void update({ outputFormat: format }, () => call(commands.setMcpOutputFormat(format)))}
              />
              <span>
                <strong>{t(`security.output.${format}`)}</strong> <span className="muted">{t(`security.output.${format}Hint`)}</span>
              </span>
            </label>
          ))}
        </fieldset>
      </section>
      <section aria-labelledby="mcp-readonly-title">
        <h2 id="mcp-readonly-title">{t("security.readOnly.title")}</h2>
        <label className="mcp-switch">
          <input
            type="checkbox"
            role="switch"
            checked={settings?.readOnly ?? false}
            disabled={!settings}
            onChange={(e) => {
              const on = e.currentTarget.checked;
              void update({ readOnly: on }, () => call(commands.setMcpReadOnly(on)));
            }}
          />
          <span>{t("security.readOnly.switch")}</span>
        </label>
        <p className="muted">{t("security.readOnly.hint")}</p>
        <table className="mcp-access" aria-label={t("security.access.title")}>
          <thead>
            <tr>
              <th>{t("security.access.connection")}</th>
              <th>{t("security.access.agents")}</th>
              <th>{t("security.access.type")}</th>
            </tr>
          </thead>
          <tbody>
            {access?.length === 0 && (
              <tr>
                <td colSpan={3} className="muted">{t("security.access.none")}</td>
              </tr>
            )}
            {access?.map((c) => (
              <tr key={c.id} data-connection={c.id}>
                <td>
                  <div>{c.name}</div>
                  <code className="muted">{c.id}</code>
                </td>
                <td>
                  <div className={`mcp-access-${c.visibility === "visible" ? c.access : "hidden"}`}>
                    {c.visibility === "visible" ? t(`security.access.${c.access}`) : t(`security.access.${c.visibility}`)}
                  </div>
                  {c.visibility === "visible" && c.reason && <div className="muted">{t(`security.access.because.${c.reason}`)}</div>}
                </td>
                <td>
                  {c.kind ? (
                    <>
                      <TypeBadge type={c.kind} small />
                      <div className="muted">{safeguards(c, t)}</div>
                    </>
                  ) : (
                    <span className="muted">—</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
        <p className="muted">{t("security.access.change")}</p>
      </section>
    </div>
  );
}

function safeguards(c: McpAccessItem, t: (key: string, options?: Record<string, unknown>) => string): string {
  const s = c.kind?.safeguards;
  if (!s) return "";
  const parts: string[] = [];
  if (s.confirmExecution) parts.push(t("security.safeguards.confirmExecution"));
  if (s.confirmDataChanges) parts.push(t("security.safeguards.confirmDataChanges"));
  if (s.manualCommit) parts.push(t("security.safeguards.manualCommit"));
  if (s.smartCommit) parts.push(t("security.safeguards.smartCommit"));
  if (s.closeIdleAfter) parts.push(t("security.safeguards.closeIdle", { minutes: Math.round(s.closeIdleAfter / 60) }));
  return parts.length ? parts.join(" · ") : t("security.safeguards.none");
}
