// Under a connection that could not open: when its engine is served by a plugin of ours that
// is not installed here (as the core's overview says), which plugin it needs and the way to the
// Plugins page. Installing is still the user's click there (ADR-055, point 9).

import { useEffect } from "react";
import { useTranslation } from "react-i18next";
import { useEngineInfo } from "../engines";
import { openPluginsPage, pluginNeeded, usePlugins } from "../plugins";

export function MissingPlugin({ engine, className }: { engine: string; className?: string }) {
  const { t } = useTranslation("connections");
  const overview = usePlugins((s) => s.overview);
  const info = useEngineInfo(engine);
  // The failure may be the first time anyone asked: read the overview again.
  useEffect(() => void usePlugins.getState().load(), [engine]);
  const needed = pluginNeeded(overview, engine);
  if (!needed) return null;
  return (
    <div className={`missing-plugin small ${className ?? ""}`} role="status" data-needs-plugin={needed.name}>
      <span>{t("needsPluginNote", { plugin: needed.name, engine: info?.name ?? engine })}</span>
      <button onClick={openPluginsPage}>{t("openPlugins")}</button>
    </div>
  );
}
