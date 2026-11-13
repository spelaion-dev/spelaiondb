// The Kubernetes tab of the connection form (ADR-015) and the Kubernetes part of the network
// profile editor (ADR-050): context, namespace, resource type, resource name and port, each
// list read from `kubectl` through the core. A connection may use a Kubernetes network
// profile: what the profile sets is shown and locked, and the connection sets the rest. The
// form only collects the choice; the core validates it, checks trust and runs kubectl.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type KubeEditor, type KubePort, type KubernetesDraftItem, type TunnelItem } from "../api";

const KINDS = ["svc", "pod", "deployment"] as const;
type Kind = (typeof KINDS)[number];
type FieldName = keyof KubernetesDraftItem;

export const EMPTY_KUBERNETES: KubernetesDraftItem = { context: "", namespace: "", resource: "", kubeconfig: "", kubectl: "" };

/** The connection's own values without what `profile` sets: a field is set in one place. */
export function withoutProfileFields(value: KubernetesDraftItem, profile: KubernetesDraftItem | null | undefined): KubernetesDraftItem {
  if (!profile) return value;
  const next = { ...value };
  for (const field of Object.keys(EMPTY_KUBERNETES) as FieldName[]) {
    if (profile[field]) next[field] = "";
  }
  return next;
}

/** `svc/orders` → ["svc", "orders"]; the long and short spellings kubectl accepts map to ours. */
function splitResource(resource: string): [Kind, string] {
  const [kind, ...rest] = resource.split("/");
  const name = rest.join("/");
  switch (kind) {
    case "pod":
    case "po":
    case "pods":
      return ["pod", name];
    case "deployment":
    case "deploy":
    case "deployments":
      return ["deployment", name];
    default:
      return ["svc", name];
  }
}

/** A list from kubectl: loading, loaded, or kubectl's error. */
type Loaded<T> = { kind: "idle" } | { kind: "loading" } | { kind: "ok"; items: T[] } | { kind: "error"; message: string };

/** Runs `load` whenever `key` changes (and `enabled`), dropping answers to older keys. */
function useKubectl<T>(enabled: boolean, key: string, load: () => Promise<T[]>): Loaded<T> {
  const [state, setState] = useState<Loaded<T>>({ kind: "idle" });
  useEffect(() => {
    if (!enabled) {
      setState({ kind: "idle" });
      return;
    }
    let current = true;
    setState({ kind: "loading" });
    load().then(
      (items) => current && setState({ kind: "ok", items }),
      (e: unknown) => current && setState({ kind: "error", message: e instanceof Error ? e.message : String(e) }),
    );
    return () => {
      current = false;
    };
    // `load` is rebuilt on every render; `key` says when its inputs changed.
  }, [enabled, key]);
  return state;
}

/** Options for a select: the loaded items plus the current value, so it always shows. */
function withCurrent(state: Loaded<string>, current: string): string[] {
  const items = state.kind === "ok" ? state.items : [];
  return current && !items.includes(current) ? [current, ...items] : items;
}

type ConnectionProps = {
  mode: "connection";
  enabled: boolean;
  onEnabled(enabled: boolean): void;
  editing: string | null;
  /** The connection's port: the resource's port when going through Kubernetes. */
  port: number | null;
  defaultPort: number | null;
  onPort(port: number): void;
  /** Kubernetes network profiles the connection may use, and the one it uses ("" for none). */
  profiles: TunnelItem[];
  profile: string;
  onProfile(name: string): void;
  /** New and Edit buttons for the profiles, next to the picker. */
  profileActions: ReactNode;
};

type ProfileProps = {
  mode: "profile";
  /** The profile being edited, by name; null for a new one. */
  editing: string | null;
};

export function KubernetesTab(
  props: {
    personal: boolean;
    trusted: boolean;
    value: KubernetesDraftItem;
    onChange(value: KubernetesDraftItem): void;
  } & (ConnectionProps | ProfileProps),
) {
  const { t } = useTranslation("connections");
  const { value, onChange } = props;
  const connection = props.mode === "connection" ? props : null;
  const enabled = connection ? connection.enabled : true;
  const shared = connection ? (connection.profiles.find((p) => p.name === connection.profile)?.kubernetes ?? null) : null;
  const locked = (field: FieldName) => Boolean(shared?.[field]);
  /** What a field shows: the profile's value where it sets one, else the connection's own. */
  const shown = (field: FieldName) => (shared?.[field] ? shared[field] : value[field]);
  const resource = shown("resource");
  const [kind, setKind] = useState<Kind>(() => splitResource(resource)[0]);
  useEffect(() => {
    if (locked("resource")) setKind(splitResource(resource)[0]);
  }, [resource]);
  const name = resource ? splitResource(resource)[1] : "";
  // Typed paths are only sent to kubectl when the field is left, not on every key.
  const [tools, setTools] = useState(() => `${value.kubeconfig}|${value.kubectl}`);
  const commitTools = () => setTools(`${value.kubeconfig}|${value.kubectl}`);
  // The port follows the resource only when the user picks one, never when a saved
  // connection is opened.
  const picked = useRef(false);

  /** A change resets what depends on it, except what the profile sets. */
  const set = (patch: Partial<KubernetesDraftItem>) => onChange(withoutProfileFields({ ...value, ...patch }, shared));
  const editor: KubeEditor = connection
    ? { kind: "connection", personal: props.personal, editing: connection.editing, profile: connection.profile || null }
    : { kind: "profile", personal: props.personal, editing: props.editing };
  const profileKey = connection ? connection.profile : "";
  const provenance = `${props.personal}|${props.editing}|${props.trusted}|${profileKey}`;
  const contexts = useKubectl(enabled, `${provenance}|${tools}`, () => call(commands.kubeContexts(value, editor)));
  const namespaces = useKubectl(enabled, `${provenance}|${tools}|${value.context}`, () => call(commands.kubeNamespaces(value, editor)));
  const names = useKubectl(enabled, `${provenance}|${tools}|${value.context}|${value.namespace}|${kind}`, () => call(commands.kubeNames(value, kind, editor)));
  const ports = useKubectl<KubePort>(enabled && Boolean(connection) && Boolean(resource), `${provenance}|${tools}|${value.context}|${value.namespace}|${resource}`, () =>
    call(commands.kubePorts(value, editor)),
  );

  const currentPort = connection ? (connection.port ?? connection.defaultPort) : null;
  const onPort = connection?.onPort;
  useEffect(() => {
    if (ports.kind !== "ok" || !picked.current || !onPort) return;
    picked.current = false;
    const first = ports.items[0];
    if (first && !ports.items.some((p) => p.port === currentPort)) onPort(first.port);
  }, [ports, currentPort, onPort]);

  const error = [contexts, namespaces, names, ports].find((s) => s.kind === "error");
  const loading = [contexts, namespaces, names, ports].some((s) => s.kind === "loading");
  const portLabel = (p: KubePort) => {
    const detail = [p.name, p.container].filter(Boolean).join(" · ");
    return detail ? t("kubernetes.portContainer", { port: p.port, detail }) : String(p.port);
  };
  const off = (field: FieldName) => !enabled || locked(field);

  const fields = (
    <>
      <div className="form-pair">
        <Select label={t("kubernetes.context")} value={shown("context")} disabled={off("context")} onChange={(context) => set({ context, namespace: "", resource: "" })}>
          <option value="">{t("kubernetes.currentContext")}</option>
          {withCurrent(contexts, shown("context")).map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </Select>
        <Select label={t("kubernetes.namespace")} value={shown("namespace")} disabled={off("namespace")} onChange={(namespace) => set({ namespace, resource: "" })}>
          <option value="">{t("kubernetes.defaultNamespace")}</option>
          {withCurrent(namespaces, shown("namespace")).map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </Select>
      </div>
      <div className="form-pair">
        <Select
          label={t("kubernetes.kind")}
          value={kind}
          disabled={off("resource")}
          onChange={(next) => {
            setKind(next as Kind);
            set({ resource: "" });
          }}
        >
          {KINDS.map((k) => (
            <option key={k} value={k}>
              {t(`kubernetes.kinds.${k}`)}
            </option>
          ))}
        </Select>
        <Select
          label={t("kubernetes.name")}
          value={name}
          disabled={off("resource")}
          onChange={(next) => {
            picked.current = true;
            set({ resource: next ? `${kind}/${next}` : "" });
          }}
        >
          <option value="">{connection ? t("kubernetes.choose") : t("kubernetes.eachConnection")}</option>
          {withCurrent(names, name).map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </Select>
        {!connection || (ports.kind === "ok" && ports.items.length === 0) ? null : (
          <Select label={t("kubernetes.port")} value={currentPort ? String(currentPort) : ""} disabled={!enabled || !resource} className="port" onChange={(p) => connection.onPort(Number(p))}>
            {currentPort && !(ports.kind === "ok" && ports.items.some((p) => p.port === currentPort)) && <option value={String(currentPort)}>{currentPort}</option>}
            {(ports.kind === "ok" ? ports.items : []).map((p) => (
              <option key={`${p.container ?? ""}:${p.port}`} value={String(p.port)}>
                {portLabel(p)}
              </option>
            ))}
          </Select>
        )}
      </div>
      {connection && ports.kind === "ok" && ports.items.length === 0 && <p className="muted small">{t("kubernetes.noPorts")}</p>}
      <div className="form-pair">
        <label className="field grow">
          <span>{t("kubernetes.kubeconfig")}</span>
          <input
            type="text"
            value={shown("kubeconfig")}
            disabled={off("kubeconfig")}
            placeholder={t("kubernetes.kubeconfigPlaceholder")}
            onChange={(e) => set({ kubeconfig: e.target.value })}
            onBlur={commitTools}
          />
        </label>
        <label className="field">
          <span>{t("kubernetes.kubectl")}</span>
          <input type="text" value={shown("kubectl")} disabled={off("kubectl")} placeholder="kubectl" onChange={(e) => set({ kubectl: e.target.value })} onBlur={commitTools} />
        </label>
      </div>
      <div role="status">
        {error?.kind === "error" ? (
          <p className="error small">{error.message}</p>
        ) : (
          loading && <p className="muted small">{t("kubernetes.loading")}</p>
        )}
      </div>
    </>
  );

  if (!connection) {
    return (
      <fieldset aria-label={t("sections.kubernetes")}>
        <legend>{t("sections.kubernetes")}</legend>
        {fields}
        <p className="muted small">{t("kubernetes.profileHint")}</p>
      </fieldset>
    );
  }

  return (
    <fieldset>
      <legend>{t("sections.kubernetes")}</legend>
      <label className="check">
        <input type="checkbox" checked={enabled} onChange={(e) => connection.onEnabled(e.target.checked)} />
        {t("kubernetes.use")}
      </label>
      <div className="form-pair">
        <Select label={t("kubernetes.profile")} value={connection.profile} disabled={!enabled} onChange={connection.onProfile}>
          <option value="">{t("kubernetes.noProfile")}</option>
          {connection.profiles.map((p) => (
            <option key={`${p.personal}:${p.name}`} value={p.name}>
              {p.name}
              {p.personal ? ` (${t("fields.personal")})` : ""}
            </option>
          ))}
        </Select>
        {connection.profileActions}
      </div>
      <p className="muted small">{connection.profile ? t("kubernetes.fromProfile") : t("kubernetes.profilesHint")}</p>
      {fields}
      <p className="muted small">{t("kubernetes.hint")}</p>
      <p className="muted small">{t("kubernetes.exclusive")}</p>
    </fieldset>
  );
}

function Select(props: { label: string; value: string; disabled: boolean; className?: string; onChange(value: string): void; children: React.ReactNode }) {
  return (
    <label className={`field ${props.className ?? "grow"}`}>
      <span>{props.label}</span>
      <select aria-label={props.label} value={props.value} disabled={props.disabled} onChange={(e) => props.onChange(e.target.value)}>
        {props.children}
      </select>
    </label>
  );
}
