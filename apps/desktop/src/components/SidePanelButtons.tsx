// "Hide side panel" (SDB-451), at the right end of the header of each panel that shares the side
// column. It runs "Toggle side panel" (Ctrl+B) and shows its keys as the user set them (#455);
// the activity bar's icon and Ctrl+B show the panel again.

import { useTranslation } from "react-i18next";
import { activeContext, runCommand } from "../commands";
import { commandTitle } from "../shortcuts";
import { Icon } from "./Icons";

export function HideSidePanelButton() {
  const { t } = useTranslation("app");
  return (
    <button
      type="button"
      className="ghost icon-button side-panel-toggle"
      aria-label={t("sidePanel.hide")}
      title={commandTitle(t("sidePanel.hide"), "view.sidePanel")}
      onClick={() => runCommand("view.sidePanel", activeContext("menu"))}
    >
      <Icon name="panelHide" />
    </button>
  );
}
