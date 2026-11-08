// The tab bar's file button (ADR-036): open a `.sql` file and the recent files. Reopening a
// closed tab is the tab menu's (Ctrl+Shift+T); recent files are a separate, persistent list.

import { useEffect, useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { RecentFileItem } from "../api";
import { Icon } from "./Icons";
import { RECENT_FILES_EVENT, forgetRecentFile, openFilePath, recentFiles } from "../sqlFiles";
import { CommandMenuItem } from "./CommandMenuItem";

export function FileMenuButton() {
  const { t } = useTranslation("app");
  const [open, setOpen] = useState<{ x: number; y: number } | null>(null);
  const button = useRef<HTMLButtonElement>(null);
  return (
    <>
      <button
        ref={button}
        className="new-tab file-menu-button"
        type="button"
        aria-haspopup="menu"
        aria-expanded={open !== null}
        aria-label={t("files.menu")}
        title={t("files.menu")}
        onClick={() => {
          const rect = button.current?.getBoundingClientRect();
          setOpen(open ? null : { x: rect?.left ?? 0, y: (rect?.bottom ?? 0) + 2 });
        }}
      >
        <Icon name="folderOpen" />
      </button>
      {open && <FileMenu {...open} onClose={() => setOpen(null)} />}
    </>
  );
}

function FileMenu({ x, y, onClose }: { x: number; y: number; onClose(): void }) {
  const { t } = useTranslation("app");
  const [recent, setRecent] = useState<RecentFileItem[] | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });

  useEffect(() => {
    let alive = true;
    const load = () => void recentFiles().then((list) => alive && setRecent(list));
    load();
    window.addEventListener(RECENT_FILES_EVENT, load);
    return () => {
      alive = false;
      window.removeEventListener(RECENT_FILES_EVENT, load);
    };
  }, []);

  useLayoutEffect(() => {
    const rect = ref.current?.getBoundingClientRect();
    if (!rect) return;
    setPosition({
      left: Math.max(8, Math.min(x, window.innerWidth - rect.width - 8)),
      top: Math.max(8, Math.min(y, window.innerHeight - rect.height - 8)),
    });
  }, [x, y, recent]);

  useEffect(() => {
    ref.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
    const close = (e: MouseEvent) => {
      if (!(e.target as Element | null)?.closest(".file-menu, .file-menu-button")) onClose();
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
      className="grid-menu tab-menu file-menu"
      role="menu"
      aria-label={t("files.menu")}
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
      <CommandMenuItem id="file.open" tab={null} onClose={onClose} />
      <hr />
      <div className="menu-heading muted small">{t("files.recent")}</div>
      {recent !== null && recent.length === 0 && <div className="menu-empty muted small">{t("files.noRecent")}</div>}
      {/* The palette lists the recent files too (CommandPalette.tsx). */}
      {recent?.map((file) => (
        <div key={file.path} className="recent-file">
          <button
            role="menuitem"
            disabled={!file.exists}
            title={file.exists ? file.path : t("files.missing", { path: file.path })}
            onClick={() => {
              onClose();
              void openFilePath(file.path);
            }}
          >
            <span>{file.name}</span>
            <span className="menu-shortcut recent-path">{file.exists ? file.path : t("files.missingShort")}</span>
          </button>
          <button
            className="ghost small recent-forget"
            aria-label={t("files.forget", { name: file.name })}
            title={t("files.forget", { name: file.name })}
            onClick={() => void forgetRecentFile(file.path)}
          >
            <Icon name="x" />
          </button>
        </div>
      ))}
    </div>
  );
}
