// Names of a connection file's fields (`host`, `tls.cert`, `kubernetes.kubectl`…) as the UI
// shows them, for what the core reports by field (ADR-030).

import i18n from "./i18n";

/** The fields, translated and joined: "host, port, TLS mode". */
export function fieldList(fields: string[]): string {
  return fields.map((f) => i18n.t(`connections:fieldNames.${f}`, { defaultValue: f })).join(", ");
}
