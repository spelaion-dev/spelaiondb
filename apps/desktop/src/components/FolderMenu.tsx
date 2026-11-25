// The explorer's folder button (SDB-397, ADR-075): add a folder, close one of the open
// folders, or add one of the recent folders, each removable from the list. Adding goes
// through `addFolder` (it closes nothing, so it asks nothing; the folder then goes through its
// own repository trust); closing through `closeFolder`, which asks about what closing that
// folder's sessions would lose.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { RecentFolderItem } from "../bindings";
import { Icon } from "./Icons";
import { RECENT_FOLDERS_EVENT, addFolder, closeFolder, folderNames, forgetRecentFolder, isOpenFolder, recentFolders } from "../folders";
import { useApp } from "../store";

export function FolderMenuButton({ disabled, onOpenFolder }: { disabled: boolean; onOpenFolder(): void }) {
  const { t } = useTranslation("shell");
  const [open, setOpen] = useState<{ x: number; y: number } | null>(null);
  const button = useRef<HTMLButtonElement>(null);
  return (
    <>
      <button
        ref={button}
        className="ghost folder-menu-button"
        type="button"
        aria-haspopup="menu"
        aria-expanded={open !== null}
        title={t("openAnotherFolder")}
        disabled={disabled}
        onClick={() => {
          const rect = button.current?.getBoundingClientRect();
          setOpen(open ? null : { x: rect?.left ?? 0, y: (rect?.bottom ?? 0) + 2 });
        }}
      >
        {t("openFolder")}
      </button>
      {open && <FolderMenu {...open} onOpenFolder={onOpenFolder} onClose={() => setOpen(null)} />}
    </>
  );
}

function FolderMenu({ x, y, onOpenFolder, onClose }: { x: number; y: number; onOpenFolder(): void; onClose(): void }) {
  const { t } = useTranslation("shell");
  const [recent, setRecent] = useState<RecentFolderItem[] | null>(null);
  const openFolders = useApp((s) => s.openFolders);
  const names = folderNames(openFolders);
  const ref = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });

  useEffect(() => {
    let alive = true;
    const load = () => void recentFolders().then((list) => alive && setRecent(list.filter((folder) => !isOpenFolder(folder.path))));
    load();
    window.addEventListener(RECENT_FOLDERS_EVENT, load);
    return () => {
      alive = false;
      window.removeEventListener(RECENT_FOLDERS_EVENT, load);
    };
  }, []);

  useLayoutEffect(() => {
    const rect = ref.current?.getBoundingClientRect();
    if (!rect) return;
    setPosition({
      left: Math.max(8, Math.min(x, window.innerWidth - rect.width - 8)),
      top: Math.max(8, Math.min(y, window.innerHeight - rect.height - 8)),
    });
  }, [x, y, recent, openFolders]);

  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
    const close = (e: MouseEvent) => {
      if (!(e.target as Element | null)?.closest(".folder-menu, .folder-menu-button")) onClose();
    };
    window.addEventListener("mousedown", close);
    window.addEventListener("blur", onClose);
    return () => {
      window.removeEventListener("mousedown", close);
      window.removeEventListener("blur", onClose);
    };
  }, [onClose]);

  return (
    <div
      ref={ref}
      className="grid-menu tab-menu file-menu folder-menu"
      role="menu"
      aria-label={t("folderMenu")}
      style={{ position: "fixed", left: position.left, top: position.top }}
      onKeyDown={(e) => {
        if (e.key === "Escape") {
          e.preventDefault();
          e.stopPropagation();
          onClose();
          return;
        }
        if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
        e.preventDefault();
        const items = [...(ref.current?.querySelectorAll<HTMLButtonElement>("button[role=menuitem]:not(:disabled)") ?? [])];
        const at = items.indexOf(document.activeElement as HTMLButtonElement);
        items[e.key === "ArrowDown" ? (at + 1) % items.length : (at + items.length - 1) % items.length]?.focus();
      }}
    >
      <button role="menuitem" onClick={() => { onClose(); onOpenFolder(); }}>
        <span>{t("openFolderMenu")}</span>
      </button>
      {openFolders.length > 0 && (
        <>
          <hr />
          <div className="menu-heading muted small">{t("openFolders")}</div>
          {openFolders.map((folder) => {
            const name = names.get(folder.root) ?? folder.path;
            return (
              <div key={folder.root} className="recent-file">
                <span className="open-folder-name" title={folder.path}>
                  <span>{name}</span>
                  <span className="menu-shortcut recent-path">{folder.path}</span>
                </span>
                <button
                  role="menuitem"
                  className="ghost small recent-forget"
                  aria-label={t("closeFolder", { name })}
                  title={t("closeFolder", { name })}
                  onClick={() => {
                    onClose();
                    void closeFolder(folder.root);
                  }}
                >
                  <Icon name="x" />
                </button>
              </div>
            );
          })}
        </>
      )}
      <hr />
      <div className="menu-heading muted small">{t("recentFolders")}</div>
      {recent !== null && recent.length === 0 && <div className="menu-empty muted small">{t("noRecentFolders")}</div>}
      {/* The palette lists them too (CommandPalette.tsx). */}
      {recent?.map((folder) => (
        <div key={folder.path} className="recent-file">
          <button
            role="menuitem"
            disabled={!folder.exists}
            title={folder.exists ? folder.path : t("recentFolderMissing", { path: folder.path })}
            onClick={() => {
              onClose();
              void addFolder(folder.path);
            }}
          >
            <span>{folder.name}</span>
            <span className="menu-shortcut recent-path">{folder.exists ? folder.path : t("recentFolderMissingShort")}</span>
          </button>
          <button
            className="ghost small recent-forget"
            aria-label={t("forgetRecentFolder", { name: folder.name })}
            title={t("forgetRecentFolder", { name: folder.name })}
            onClick={() => void forgetRecentFolder(folder.path)}
          >
            <Icon name="x" />
          </button>
        </div>
      ))}
    </div>
  );
}
