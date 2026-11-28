// Settings → Plugins, "Plugins you added" (ADR-041 point 5, ADR-055 point 5): plugins of other
// people's that the user adds themselves, from a file or from an address typed or pasted here
// and nowhere else. They are not part of SpelaionDB and the project does not support them:
// this section says so, offers no updates, and its engines get no read-only mode and nothing for
// agents. Every rule is the core's (`spelaion_core::plugins::external`, through
// `src-tauri/src/plugins.rs`): this page shows what the core read, and gives back only the hash
// its dialog showed.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type PendingPluginItem, type TheirPluginItem } from "../api";
import { chooseOpenFile } from "../dialogs";
import { startJob, type Job } from "../jobs";
import { byteSize, usePlugins } from "../plugins";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** What adding a plugin is doing now. */
type Adding =
  | { kind: "idle" }
  | { kind: "readingFile" }
  | { kind: "readingAddress"; job: Job }
  | { kind: "asking"; pending: PendingPluginItem }
  | { kind: "failed"; detail: string };

export function TheirPlugins({ plugins }: { plugins: TheirPluginItem[] }) {
  const { t } = useTranslation("settings");
  const setOverview = usePlugins((s) => s.set);
  const [adding, setAdding] = useState<Adding>({ kind: "idle" });
  const [address, setAddress] = useState("");
  const [added, setAdded] = useState<string | null>(null);
  const [rowErrors, setRowErrors] = useState<Record<string, string>>({});
  const [trusting, setTrusting] = useState<{ plugin: TheirPluginItem; found: string } | null>(null);
  const [removing, setRemoving] = useState<{ plugin: TheirPluginItem; openSessions: number } | null>(null);
  const busy = adding.kind === "readingFile" || adding.kind === "readingAddress";

  function rowError(name: string, message: string | null) {
    setRowErrors((current) => {
      const next = { ...current };
      if (message === null) delete next[name];
      else next[name] = message;
      return next;
    });
  }

  async function fromFile() {
    setAdded(null);
    const path = await chooseOpenFile(t("plugins.theirs.fileTitle"), [{ name: t("plugins.theirs.fileFilter"), extensions: ["gz", "tgz"] }]);
    if (!path) return;
    setAdding({ kind: "readingFile" });
    try {
      setAdding({ kind: "asking", pending: await call(commands.pluginsInspectFile(path)) });
    } catch (e) {
      setAdding({ kind: "failed", detail: errorText(e) });
    }
  }

  async function fromAddress() {
    setAdded(null);
    const job = startJob();
    setAdding({ kind: "readingAddress", job });
    try {
      // What the user typed or pasted, as it is: the core checks it (HTTPS only) and reads it
      // once.
      setAdding({ kind: "asking", pending: await call(commands.pluginsInspectAddress(job.channel, address)) });
    } catch (e) {
      setAdding({ kind: "failed", detail: errorText(e) });
    }
  }

  /** The user's answer to the dialog. */
  async function answer(pending: PendingPluginItem, add: boolean) {
    if (!add) {
      setAdding({ kind: "idle" });
      await call(commands.pluginsDiscardTheirs(pending.archiveSha256)).catch(() => undefined);
      return;
    }
    // A failure stays in the dialog (it throws there).
    setOverview(await call(commands.pluginsAddTheirs(pending.archiveSha256)));
    setAdding({ kind: "idle" });
    setAddress("");
    setAdded(pending.engineLabel);
  }

  async function trustAgain(plugin: TheirPluginItem, found: string) {
    setOverview(await call(commands.pluginsTrustAgain(plugin.name, found)));
    setTrusting(null);
  }

  async function remove(plugin: TheirPluginItem, confirmed: boolean) {
    rowError(plugin.name, null);
    try {
      const outcome = await call(commands.pluginsRemoveTheirs(plugin.name, confirmed));
      switch (outcome.kind) {
        case "needsConfirmation":
          setRemoving({ plugin, openSessions: outcome.openSessions });
          return;
        case "removed":
          setOverview(outcome.overview);
          setRemoving(null);
          return;
      }
    } catch (e) {
      const message = t("plugins.removeError", { detail: errorText(e) });
      if (confirmed) throw new Error(message);
      rowError(plugin.name, message);
    }
  }

  return (
    <section className="their-plugins" aria-labelledby="their-plugins-title">
      <h3 id="their-plugins-title">{t("plugins.theirs.heading")}</h3>
      <p className="muted small">{t("plugins.theirs.intro")}</p>
      {plugins.length === 0 ? (
        <p className="muted small">{t("plugins.theirs.none")}</p>
      ) : (
        <ul className="plugin-list" aria-label={t("plugins.theirs.listLabel")}>
          {plugins.map((plugin) => (
            <TheirRow
              key={plugin.name}
              plugin={plugin}
              error={rowErrors[plugin.name] ?? null}
              onTrustAgain={(found) => setTrusting({ plugin, found })}
              onRemove={() => void remove(plugin, false)}
            />
          ))}
        </ul>
      )}
      <div className="form-row">
        <button onClick={() => void fromFile()} disabled={busy}>
          {adding.kind === "readingFile" ? t("plugins.theirs.reading") : t("plugins.theirs.fromFile")}
        </button>
      </div>
      <form
        className="their-plugin-address"
        onSubmit={(e) => {
          e.preventDefault();
          if (address.trim()) void fromAddress();
        }}
      >
        <label htmlFor="their-plugin-address">{t("plugins.theirs.addressLabel")}</label>
        <div className="form-row">
          <input
            id="their-plugin-address"
            type="url"
            inputMode="url"
            placeholder="https://"
            spellCheck={false}
            autoComplete="off"
            value={address}
            disabled={busy}
            onChange={(e) => {
              setAddress(e.target.value);
              if (adding.kind === "failed") setAdding({ kind: "idle" });
            }}
          />
          {adding.kind === "readingAddress" ? (
            <button type="button" onClick={() => adding.job.cancel()}>
              {t("plugins.theirs.cancelReading")}
            </button>
          ) : (
            <button type="submit" disabled={busy || !address.trim()}>
              {t("plugins.theirs.readAddress")}
            </button>
          )}
        </div>
        <p className="muted small">{t("plugins.theirs.addressHint")}</p>
      </form>
      <div role="status" aria-live="polite">
        {adding.kind === "readingAddress" && <p className="muted small">{t("plugins.theirs.readingAddress")}</p>}
        {added && <p className="muted small">{t("plugins.theirs.added", { engine: added })}</p>}
      </div>
      {adding.kind === "failed" && (
        <p className="error small" role="alert">
          {t("plugins.theirs.readError", { detail: adding.detail })}
        </p>
      )}
      {adding.kind === "asking" && (
        <AddDialog pending={adding.pending} onCancel={() => void answer(adding.pending, false)} onAdd={() => answer(adding.pending, true)} />
      )}
      {trusting && (
        <TrustAgainDialog
          plugin={trusting.plugin}
          found={trusting.found}
          onCancel={() => setTrusting(null)}
          onTrust={() => trustAgain(trusting.plugin, trusting.found)}
        />
      )}
      {removing && (
        <RemoveTheirsDialog
          plugin={removing.plugin}
          openSessions={removing.openSessions}
          onCancel={() => setRemoving(null)}
          onRemove={() => remove(removing.plugin, true)}
        />
      )}
    </section>
  );
}

function TheirRow({
  plugin,
  error,
  onTrustAgain,
  onRemove,
}: {
  plugin: TheirPluginItem;
  error: string | null;
  onTrustAgain(found: string): void;
  onRemove(): void;
}) {
  const { t } = useTranslation("settings");
  const title = plugin.engineLabel ?? plugin.name;
  const state = plugin.state;
  return (
    <li className="plugin-row" aria-label={title} data-their-plugin={plugin.name}>
      <div className="plugin-main">
        <div className="plugin-title">
          <span className="plugin-name">{title}</span>
          <span className="tag" title={t("plugins.theirs.notOursHint")}>
            {t("plugins.theirs.notOurs")}
          </span>
          <span className="mono small muted">
            {plugin.version ? `${plugin.name} ${plugin.version}` : plugin.name}
          </span>
        </div>
        {plugin.author && <div className="small">{t("plugins.theirs.saysBy", { author: plugin.author })}</div>}
        {plugin.description && <div className="small muted">{plugin.description}</div>}
        {plugin.source && <div className="small muted">{t("plugins.theirs.addedFrom", { source: plugin.source })}</div>}
        {plugin.homepage && <div className="small muted">{t("plugins.theirs.homepage", { address: plugin.homepage })}</div>}
        {state.kind === "trusted" && plugin.sha256 && (
          <div className="small muted mono">{t("plugins.theirs.trustedHash", { sha256: plugin.sha256 })}</div>
        )}
        {plugin.refused && <div className="small plugin-why">{plugin.refused}</div>}
        {plugin.openSessions > 0 && <div className="small muted">{t("plugins.sessions", { count: plugin.openSessions })}</div>}
        {error && (
          <div className="error small" role="alert">
            {error}
          </div>
        )}
      </div>
      <div className="plugin-actions">
        {state.kind === "changed" && <button onClick={() => onTrustAgain(state.found)}>{t("plugins.theirs.trustAgain")}</button>}
        <button onClick={onRemove}>{t("plugins.remove")}</button>
      </div>
    </li>
  );
}

/** The one place someone else's plugin is let in: what it is, who it says it is from, the
 *  engine it adds, where it came from and the hash of what arrived. Cancel has the focus. */
function AddDialog({ pending, onCancel, onAdd }: { pending: PendingPluginItem; onCancel(): void; onAdd(): Promise<void> }) {
  const { t, i18n } = useTranslation("settings");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => cancel.current?.focus(), []);
  const origin = pending.origin;

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      await onAdd();
    } catch (e) {
      setError(t("plugins.theirs.addError", { detail: errorText(e) }));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div
        className="modal their-plugin-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="their-plugin-add-title"
        aria-describedby="their-plugin-add-warning"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            if (!busy) onCancel();
          }
        }}
      >
        <h2 id="their-plugin-add-title">{t("plugins.theirs.addTitle", { engine: pending.engineLabel })}</h2>
        <p id="their-plugin-add-warning">{t("plugins.theirs.addWarning")}</p>
        <dl className="their-plugin-facts">
          <dt>{t("plugins.theirs.factPlugin")}</dt>
          <dd className="mono">{`${pending.name} ${pending.version}`}</dd>
          <dt>{t("plugins.theirs.factAuthor")}</dt>
          <dd>{pending.author}</dd>
          <dt>{t("plugins.theirs.factEngine")}</dt>
          <dd>
            {pending.engineLabel} <span className="mono muted">({pending.engine})</span>
          </dd>
          <dt>{t("plugins.theirs.factDialect")}</dt>
          <dd>{pending.dialect ? t("plugins.theirs.dialectNamed", { dialect: pending.dialect }) : t("plugins.theirs.dialectNone")}</dd>
          {pending.description && (
            <>
              <dt>{t("plugins.theirs.factDescription")}</dt>
              <dd>{pending.description}</dd>
            </>
          )}
          {origin.kind === "file" ? (
            <>
              <dt>{t("plugins.theirs.factFile")}</dt>
              <dd className="mono">{origin.path}</dd>
            </>
          ) : (
            <>
              <dt>{t("plugins.theirs.factAddress")}</dt>
              <dd className="mono">{origin.address}</dd>
              <dt>{t("plugins.theirs.factHost")}</dt>
              <dd className="mono">{origin.host}</dd>
              {origin.archive && (
                <>
                  <dt>{t("plugins.theirs.factArchive")}</dt>
                  <dd className="mono">{origin.archive}</dd>
                  <dt>{t("plugins.theirs.factArchiveHost")}</dt>
                  <dd className="mono">{origin.archiveHost}</dd>
                </>
              )}
              {origin.arrivedFrom && (
                <>
                  <dt>{t("plugins.theirs.factArrivedFrom")}</dt>
                  <dd className="mono">{origin.arrivedFrom}</dd>
                </>
              )}
            </>
          )}
          <dt>{t("plugins.theirs.factSize")}</dt>
          <dd>{byteSize(pending.archiveSize ?? 0, i18n.language)}</dd>
          <dt>{t("plugins.theirs.factArchiveHash")}</dt>
          <dd className="mono">{pending.archiveSha256}</dd>
          <dt>{t("plugins.theirs.factProgramHash")}</dt>
          <dd className="mono">{pending.programSha256}</dd>
        </dl>
        <p className="small">{t("plugins.theirs.addUnsupported")}</p>
        {pending.replaces && <p className="small">{t("plugins.theirs.addReplaces", { version: pending.replaces })}</p>}
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button ref={cancel} onClick={onCancel} disabled={busy}>
            {t("plugins.cancel")}
          </button>
          <button className="primary" onClick={() => void confirm()} disabled={busy}>
            {busy ? t("plugins.theirs.adding") : t("plugins.theirs.addConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** A plugin whose program changed since the user trusted it: the hash it had and the hash it
 *  has now. Nothing starts it until the user trusts this file. */
function TrustAgainDialog({
  plugin,
  found,
  onCancel,
  onTrust,
}: {
  plugin: TheirPluginItem;
  found: string;
  onCancel(): void;
  onTrust(): Promise<void>;
}) {
  const { t } = useTranslation("settings");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => cancel.current?.focus(), []);

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      await onTrust();
    } catch (e) {
      setError(t("plugins.theirs.trustError", { detail: errorText(e) }));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div
        className="modal their-plugin-dialog"
        role="dialog"
        aria-modal="true"
        aria-labelledby="their-plugin-trust-title"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            if (!busy) onCancel();
          }
        }}
      >
        <h2 id="their-plugin-trust-title">{t("plugins.theirs.trustTitle", { name: plugin.name })}</h2>
        <p>{t("plugins.theirs.trustBody")}</p>
        <dl className="their-plugin-facts">
          <dt>{t("plugins.theirs.trustWas")}</dt>
          <dd className="mono">{plugin.sha256}</dd>
          <dt>{t("plugins.theirs.trustIs")}</dt>
          <dd className="mono">{found}</dd>
        </dl>
        <p className="muted small">{t("plugins.theirs.trustHint")}</p>
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button ref={cancel} onClick={onCancel} disabled={busy}>
            {t("plugins.cancel")}
          </button>
          <button className="primary" onClick={() => void confirm()} disabled={busy}>
            {busy ? t("plugins.theirs.trusting") : t("plugins.theirs.trustConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}

/** Removing a plugin whose engine has sessions open: they are lost (ADR-028). */
function RemoveTheirsDialog({
  plugin,
  openSessions,
  onCancel,
  onRemove,
}: {
  plugin: TheirPluginItem;
  openSessions: number;
  onCancel(): void;
  onRemove(): Promise<void>;
}) {
  const { t } = useTranslation("settings");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const cancel = useRef<HTMLButtonElement>(null);
  useEffect(() => cancel.current?.focus(), []);

  async function confirm() {
    setBusy(true);
    setError(null);
    try {
      await onRemove();
    } catch (e) {
      setError(errorText(e));
      setBusy(false);
    }
  }

  return (
    <div className="modal-backdrop">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-labelledby="their-plugin-remove-title"
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            if (!busy) onCancel();
          }
        }}
      >
        <h2 id="their-plugin-remove-title">{t("plugins.theirs.removeTitle", { name: plugin.name })}</h2>
        <p>{t("plugins.removeSessions", { count: openSessions, engines: plugin.engineLabel ?? plugin.name })}</p>
        <p className="muted small">{t("plugins.theirs.removeHint")}</p>
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button ref={cancel} onClick={onCancel} disabled={busy}>
            {t("plugins.cancel")}
          </button>
          <button className="danger" onClick={() => void confirm()} disabled={busy}>
            {busy ? t("plugins.removing") : t("plugins.removeConfirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
