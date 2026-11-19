// The update's install button and what went wrong, the same in the notice, the release notes
// and Settings (SDB-377): both read the state the core keeps (`src/update.ts`).

import { useTranslation } from "react-i18next";
import { downloadText, installOf, isInstalling, updateProblem, useUpdate } from "../update";

/** The texts' namespace and group: the notice and the release notes use `app`'s `update`,
 * Settings its own `updates`. */
interface Texts {
  namespace: "app" | "settings";
  group: "update" | "updates";
}

/** "Install and restart"; while the update downloads, how far it is (disabled). */
export function UpdateInstallButton({ namespace, group }: Texts) {
  const { t, i18n } = useTranslation(namespace);
  const status = useUpdate((s) => s.status);
  const install = useUpdate((s) => s.install);
  const phase = installOf(status);
  const progress = phase ? downloadText(phase, i18n.language) : null;
  return (
    <button className="primary" disabled={isInstalling(status)} onClick={() => void install()}>
      {progress ? t(`${group}.${progress.key}`, progress.values) : t(`${group}.install`)}
    </button>
  );
}

/** What failed, worded by what was being done: a failed install says it can be tried again. */
export function UpdateProblem({ namespace, group, className }: Texts & { className: string }) {
  const { t } = useTranslation(namespace);
  const status = useUpdate((s) => s.status);
  const request = useUpdate((s) => s.request);
  const problem = updateProblem({ status, request });
  if (!problem) return null;
  return (
    <p className={className} role="alert">
      {t(`${group}.failed.${problem.action}`, { detail: problem.message })}
    </p>
  );
}
