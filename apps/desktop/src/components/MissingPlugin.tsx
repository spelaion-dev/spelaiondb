// Under a connection that could not open: when its engine is served by a plugin of ours that
// is not installed here (as the core's overview says), which plugin it needs, its install when
// one is running or failed (wherever it was started), and the way to the Plugins page.
// Installing is still the user's click there (ADR-055, point 9).

import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { useEngineInfo } from "../engines";
import { installText, openPluginsPage, pluginNeeded, usePlugins } from "../plugins";

export function MissingPlugin({ engine, className }: { engine: string; className?: string }) {
  const { t } = useTranslation("connections");
  const { t: settings, i18n } = useTranslation("settings");
  const overview = usePlugins((s) => s.overview);
  const info = useEngineInfo(engine);
  // The failure may be the first time anyone asked: read the overview again.
  useEffect(() => void usePlugins.getState().load(), [engine]);
  const needed = pluginNeeded(overview, engine);
  if (!needed) return null;
  const install = installText(needed, settings, i18n.language);
  return (
    <div className={`missing-plugin small ${className ?? ""}`} role="status" data-needs-plugin={needed.name}>
      <span>{t("needsPluginNote", { plugin: needed.name, engine: info?.name ?? engine })}</span>
      {install && <span data-plugin-install={needed.install.kind}>{install}</span>}
      <button onClick={openPluginsPage}>{t("openPlugins")}</button>
    </div>
  );
}
