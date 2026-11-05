// The Kubernetes tab of the connection form (ADR-015): context, namespace, resource type,
// resource name and port, each list read from `kubectl` through the core. The form only
// collects the choice; the core validates it and runs `kubectl port-forward`.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { call, commands, type KubePort, type KubernetesDraftItem } from "../api";

const KINDS = ["svc", "pod", "deployment"] as const;
type Kind = (typeof KINDS)[number];

export const EMPTY_KUBERNETES: KubernetesDraftItem = { context: "", namespace: "", resource: "", kubeconfig: "", kubectl: "" };

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

export function KubernetesTab(props: {
  enabled: boolean;
  personal: boolean;
  editing: string | null;
  trusted: boolean;
  onEnabled(enabled: boolean): void;
  value: KubernetesDraftItem;
  onChange(value: KubernetesDraftItem): void;
  /** The connection's port: the resource's port when going through Kubernetes. */
  port: number | null;
  defaultPort: number | null;
  onPort(port: number): void;
}) {
  const { t } = useTranslation("connections");
  const { enabled, value, onChange } = props;
  const [kind, setKind] = useState<Kind>(() => splitResource(value.resource)[0]);
  const name = value.resource ? splitResource(value.resource)[1] : "";
  // Typed paths are only sent to kubectl when the field is left, not on every key.
  const [tools, setTools] = useState(() => `${value.kubeconfig}|${value.kubectl}`);
  const commitTools = () => setTools(`${value.kubeconfig}|${value.kubectl}`);
  // The port follows the resource only when the user picks one, never when a saved
  // connection is opened.
  const picked = useRef(false);

  const set = (patch: Partial<KubernetesDraftItem>) => onChange({ ...value, ...patch });
  const provenance = `${props.personal}|${props.editing}|${props.trusted}`;
  const contexts = useKubectl(enabled, `${provenance}|${tools}`, () => call(commands.kubeContexts(value, props.personal, props.editing)));
  const namespaces = useKubectl(enabled, `${provenance}|${tools}|${value.context}`, () => call(commands.kubeNamespaces(value, props.personal, props.editing)));
  const names = useKubectl(enabled, `${provenance}|${tools}|${value.context}|${value.namespace}|${kind}`, () =>
    call(commands.kubeNames(value, kind, props.personal, props.editing)),
  );
  const ports = useKubectl<KubePort>(enabled && Boolean(value.resource), `${provenance}|${tools}|${value.context}|${value.namespace}|${value.resource}`, () =>
    call(commands.kubePorts(value, props.personal, props.editing)),
  );

  const { onPort, port, defaultPort } = props;
  const currentPort = port ?? defaultPort;
  useEffect(() => {
    if (ports.kind !== "ok" || !picked.current) return;
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

  return (
    <fieldset>
      <legend>{t("sections.kubernetes")}</legend>
      <label className="check">
        <input type="checkbox" checked={enabled} onChange={(e) => props.onEnabled(e.target.checked)} />
        {t("kubernetes.use")}
      </label>
      <div className="form-pair">
        <Select label={t("kubernetes.context")} value={value.context} disabled={!enabled} onChange={(context) => set({ context, namespace: "", resource: "" })}>
          <option value="">{t("kubernetes.currentContext")}</option>
          {withCurrent(contexts, value.context).map((c) => (
            <option key={c} value={c}>
              {c}
            </option>
          ))}
        </Select>
        <Select label={t("kubernetes.namespace")} value={value.namespace} disabled={!enabled} onChange={(namespace) => set({ namespace, resource: "" })}>
          <option value="">{t("kubernetes.defaultNamespace")}</option>
          {withCurrent(namespaces, value.namespace).map((n) => (
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
          disabled={!enabled}
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
          disabled={!enabled}
          onChange={(next) => {
            picked.current = true;
            set({ resource: next ? `${kind}/${next}` : "" });
          }}
        >
          <option value="">{t("kubernetes.choose")}</option>
          {withCurrent(names, name).map((n) => (
            <option key={n} value={n}>
              {n}
            </option>
          ))}
        </Select>
        {ports.kind === "ok" && ports.items.length === 0 ? null : (
          <Select label={t("kubernetes.port")} value={currentPort ? String(currentPort) : ""} disabled={!enabled || !value.resource} className="port" onChange={(p) => onPort(Number(p))}>
            {currentPort && !(ports.kind === "ok" && ports.items.some((p) => p.port === currentPort)) && <option value={String(currentPort)}>{currentPort}</option>}
            {(ports.kind === "ok" ? ports.items : []).map((p) => (
              <option key={`${p.container ?? ""}:${p.port}`} value={String(p.port)}>
                {portLabel(p)}
              </option>
            ))}
          </Select>
        )}
      </div>
      {ports.kind === "ok" && ports.items.length === 0 && <p className="muted small">{t("kubernetes.noPorts")}</p>}
      <div className="form-pair">
        <label className="field grow">
          <span>{t("kubernetes.kubeconfig")}</span>
          <input type="text" value={value.kubeconfig} disabled={!enabled} placeholder={t("kubernetes.kubeconfigPlaceholder")} onChange={(e) => set({ kubeconfig: e.target.value })} onBlur={commitTools} />
        </label>
        <label className="field">
          <span>{t("kubernetes.kubectl")}</span>
          <input type="text" value={value.kubectl} disabled={!enabled} placeholder="kubectl" onChange={(e) => set({ kubectl: e.target.value })} onBlur={commitTools} />
        </label>
      </div>
      <div role="status">
        {error?.kind === "error" ? (
          <p className="error small">{error.message}</p>
        ) : (
          loading && <p className="muted small">{t("kubernetes.loading")}</p>
        )}
      </div>
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
