// How a connection whose TLS does not check its server is told (SDB-395). The core decides
// whether there is a warning and what it names (`TlsWarningItem`: the risk, the mode, the host
// and whether a tunnel carries the traffic); this only words it, the same in the form, the
// connection test and the explorer.

import type { TFunction } from "i18next";
import type { TlsWarningItem } from "./bindings";

/** The warning's sentence, in the `connections` namespace. */
export function tlsWarningText(t: TFunction, warning: TlsWarningItem): string {
  const key = `${warning.risk === "unencrypted" ? "unencrypted" : "unverified"}${warning.tunneled ? "Tunneled" : ""}`;
  return t(`connections:tlsWarning.${key}`, { mode: warning.mode, host: warning.host });
}
