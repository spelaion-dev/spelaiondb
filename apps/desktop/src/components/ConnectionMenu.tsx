// The context menu of a connection row in the explorer: every action on that connection, in one
// place (right click, the Menu key or Shift+F10 on the focused row). Its options live in its
// form (Edit…), not here.

import { useLayoutEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ConnectionItem } from "../api";
import { useApp } from "../store";

/** The actions that need the explorer node's own state; the rest only need the store. */
export interface ConnectionMenuActions {
  newQuery(): void;
  /** Reloads everything the explorer shows of this connection. */
  refresh(): void;
  configureFilters(): void;
  /** Absent when the engine cannot create a schema from here. */
  createSchema?: () => void;
  duplicate(): void;
  copy(): void;
  paste(): void;
}

/** The keys a menu item names: shown right-aligned and given to assistive technology. */
const SHORTCUTS = {
  openByName: { text: "Ctrl+Shift+D", aria: "Control+Shift+D" },
  refresh: { text: "F5", aria: "F5" },
  copy: { text: "Ctrl+C", aria: "Control+C" },
  paste: { text: "Ctrl+V", aria: "Control+V" },
} as const;

export function ConnectionMenu({
  connection,
  x,
  y,
  connected,
  actions,
  onClose,
}: {
  connection: ConnectionItem;
  x: number;
  y: number;
  /** Whether the explorer has opened it: finding, refreshing and creating need that. */
  connected: boolean;
  actions: ConnectionMenuActions;
  /** Closes the menu; `restoreFocus` when it was dismissed (Escape, Tab) rather than used. */
  onClose(restoreFocus: boolean): void;
}) {
  const { t } = useTranslation(["shell", "connections"]);
  const root = useRef<HTMLDivElement>(null);
  const [position, setPosition] = useState({ left: x, top: y });

  useLayoutEffect(() => {
    const rect = root.current?.getBoundingClientRect();
    if (!rect) return;
    setPosition({
      left: Math.max(8, Math.min(x, window.innerWidth - rect.width - 8)),
      top: Math.max(8, Math.min(y, window.innerHeight - rect.height - 8)),
    });
    root.current?.querySelector<HTMLButtonElement>("button:not(:disabled)")?.focus();
  }, [x, y]);

  /** A menu item: closes the menu first, then acts. */
  const item = (label: string, act: () => void, options: { shortcut?: keyof typeof SHORTCUTS; danger?: boolean } = {}) => {
    const shortcut = options.shortcut ? SHORTCUTS[options.shortcut] : undefined;
    return (
      <button
        type="button"
        role="menuitem"
        className={options.danger ? "danger-text" : undefined}
        aria-label={label}
        aria-keyshortcuts={shortcut?.aria}
        onClick={() => {
          onClose(false);
          act();
        }}
      >
        <span>{label}</span>
        <span className="menu-shortcut">{shortcut?.text ?? ""}</span>
      </button>
    );
  };

  const app = useApp.getState;
  return (
    <div
      ref={root}
      className="grid-menu tab-menu object-menu connection-menu"
      role="menu"
      aria-label={t("connectionMenu", { name: connection.name })}
      style={{ position: "fixed", left: position.left, top: position.top }}
      onKeyDown={(e) => {
        if (e.key === "Escape" || e.key === "Tab") {
          e.preventDefault();
          e.stopPropagation();
          onClose(true);
          return;
        }
        if (e.key !== "ArrowDown" && e.key !== "ArrowUp" && e.key !== "Home" && e.key !== "End") return;
        e.preventDefault();
        const items = [...(root.current?.querySelectorAll<HTMLButtonElement>("button:not(:disabled)") ?? [])];
        const at = items.indexOf(document.activeElement as HTMLButtonElement);
        const next =
          e.key === "Home"
            ? 0
            : e.key === "End"
              ? items.length - 1
              : e.key === "ArrowDown"
                ? (at + 1) % items.length
                : (at + items.length - 1) % items.length;
        items[next]?.focus();
      }}
    >
      {/* Open: what double-clicking the row does, then what needs the connection opened. */}
      {item(t("newQuery"), actions.newQuery)}
      {connected && item(t("openByName.menu"), () => app().setOpenObject({ connectionId: connection.id }), { shortcut: "openByName" })}
      {connected && item(t("refreshAllSchemas"), actions.refresh, { shortcut: "refresh" })}
      <hr />
      {/* The connection's own settings and what the explorer shows of it. */}
      {item(t("connections:edit"), () => app().setConnectionDialog({ kind: "edit", id: connection.id }))}
      {item(t("connections:configureFilters"), actions.configureFilters)}
      {connected && actions.createSchema && item(t("create.schema"), actions.createSchema)}
      <hr />
      {/* Copies and where it is kept; deleting comes last. */}
      {item(t("connections:copies.duplicate"), actions.duplicate)}
      {item(t("connections:copies.copy"), actions.copy, { shortcut: "copy" })}
      {item(t("connections:copies.paste"), actions.paste, { shortcut: "paste" })}
      {item(t("connections:move.menu"), () => app().setMovingConnection(connection))}
      <hr />
      {item(t("connections:delete"), () => app().setDeletingConnection(connection), { danger: true })}
    </div>
  );
}
