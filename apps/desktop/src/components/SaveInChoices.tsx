// Where a new or imported connection goes (ADR-075 point 8): one of the open folders that is a
// repository, or personal. With one such folder it reads as before ("Project" or "Personal");
// with several, each is a choice of its own, named as the explorer names its group. A pasted
// connection is always personal (ADR-030) and never asks.

import { useTranslation } from "react-i18next";
import type { OpenFolderItem } from "../bindings";
import { folderNames } from "../folders";
import { useApp } from "../store";

/** The open folders a repository connection can go into, in the order they were opened. */
export function repositoryFolders(folders: OpenFolderItem[]): OpenFolderItem[] {
  return folders.filter((folder) => folder.repository);
}

/** The folder offered first: the last repository folder opened (the one the page shows when it
 *  is a repository), or null when none is open. */
export function defaultRepository(folders: OpenFolderItem[]): string | null {
  const repositories = repositoryFolders(folders);
  return repositories[repositories.length - 1]?.root ?? null;
}

export function SaveInChoices({ name, personal, repository, disabled, labelClassName, onChoose }: {
  /** The radio group's name. */
  name: string;
  personal: boolean;
  /** The chosen folder's root, when not personal. */
  repository: string | null;
  /** Neither can change (an edited connection stays where it is). */
  disabled: boolean;
  labelClassName?: string;
  onChoose(choice: { personal: boolean; repository: string | null }): void;
}) {
  const { t } = useTranslation("connections");
  const openFolders = useApp((s) => s.openFolders);
  const repositories = repositoryFolders(openFolders);
  const names = folderNames(openFolders);
  const chosen = repository ?? defaultRepository(openFolders);
  return (
    <>
      {repositories.length < 2 ? (
        <label className={labelClassName}>
          <input type="radio" name={name} checked={!personal} disabled={disabled || repositories.length === 0} onChange={() => onChoose({ personal: false, repository: repositories[0]?.root ?? null })} />{" "}
          {t("fields.project")}
        </label>
      ) : (
        repositories.map((folder) => (
          <label key={folder.root} className={labelClassName} title={folder.path}>
            <input type="radio" name={name} checked={!personal && chosen === folder.root} disabled={disabled} onChange={() => onChoose({ personal: false, repository: folder.root })} />{" "}
            {names.get(folder.root) ?? folder.path}
          </label>
        ))
      )}
      <label className={labelClassName}>
        <input type="radio" name={name} checked={personal} disabled={disabled} onChange={() => onChoose({ personal: true, repository: null })} />{" "}
        {t("fields.personal")}
      </label>
    </>
  );
}
