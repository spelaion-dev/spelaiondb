// How a connection whose TLS does not check its server is told (SDB-395). The core decides
// whether there is a warning and what it names (`TlsWarningItem`: the risk, the mode, the host
// and whether a tunnel carries the traffic); this only words it, the same in the form, the
// connection test and the explorer.

import type { TFunction } from "i18next";
import type { TlsWarningItem } from "./bindings";

/** The warning's sentence, in the `connections` namespace. */
export function tlsWarningText(t: TFunction, warning: TlsWarningItem): string {
  const values = { mode: warning.mode, host: warning.host };
  const tunneled = warning.tunneled ? "Tunneled" : "";
  switch (warning.risk) {
    case "unencrypted":
      return t(`connections:tlsWarning.unencrypted${tunneled}`, values);
    case "unverified":
      return t(`connections:tlsWarning.unverified${tunneled}`, values);
    // A SQL login to LocalDB (ADR-068): no network, only the local pipe without TLS.
    case "localDbPassword":
      return t("connections:tlsWarning.localDbPassword", values);
    default: {
      const never: never = warning.risk;
      return never;
    }
  }
}
