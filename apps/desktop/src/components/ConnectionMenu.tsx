// "⋯" menu next to each connection in the sidebar: per-connection options. A future
// connection manager will take these over.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ConnectionItem } from "../api";
import { useApp } from "../store";
import { duplicateConnection } from "../connectionCopies";
import { HistoryPreference } from "./HistoryPreference";

export function ConnectionMenu({
  connection,
  onRefresh,
  onConfigureFilters,
  onError,
}: {
  connection: ConnectionItem;
  /** Reloads everything the explorer shows of this connection; absent until it is opened. */
  onRefresh?: () => void;
  /** Opens the dialog of the explorer's include and exclude patterns. */
  onConfigureFilters?: () => void;
  /** Shows what went wrong with an action of the menu. */
  onError?: (message: string) => void;
}) {
  const { t } = useTranslation(["editor", "connections"]);
  const [open, setOpen] = useState(false);
  const root = useRef<HTMLDivElement>(null);

  useEffect(() => {
    if (!open) return;
    const onPointer = (e: MouseEvent) => {
      if (!root.current?.contains(e.target as Node)) setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") setOpen(false);
    };
    document.addEventListener("mousedown", onPointer);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onPointer);
      document.removeEventListener("keydown", onKey);
    };
  }, [open]);

  return (
    <div className="connection-menu" ref={root}>
      <button
        className="ghost small"
        aria-haspopup="true"
        aria-expanded={open}
        aria-label={t("history.menu", { name: connection.name })}
        title={t("history.menu", { name: connection.name })}
        onClick={() => setOpen(!open)}
      >
        ⋯
      </button>
      {open && (
        <div className="connection-menu-popup" role="group" aria-label={t("history.menu", { name: connection.name })}>
          {onRefresh && (
            <button
              className="menu-item"
              onClick={() => {
                setOpen(false);
                onRefresh();
              }}
            >
              {t("connections:refresh")}
            </button>
          )}
          {onConfigureFilters && (
            <button
              className="menu-item"
              onClick={() => {
                setOpen(false);
                onConfigureFilters();
              }}
            >
              {t("connections:configureFilters")}
            </button>
          )}
          <button
            className="menu-item"
            onClick={() => {
              setOpen(false);
              useApp.getState().setConnectionDialog({ kind: "edit", id: connection.id });
            }}
          >
            {t("connections:edit")}
          </button>
          <button
            className="menu-item"
            onClick={() => {
              setOpen(false);
              duplicateConnection(connection).catch((e: unknown) => onError?.(e instanceof Error ? e.message : String(e)));
            }}
          >
            {t("connections:copies.duplicate")}
          </button>
          <button
            className="menu-item"
            onClick={() => {
              setOpen(false);
              useApp.getState().setMovingConnection(connection);
            }}
          >
            {t("connections:move.menu")}
          </button>
          <button
            className="menu-item danger-text"
            onClick={() => {
              setOpen(false);
              useApp.getState().setDeletingConnection(connection);
            }}
          >
            {t("connections:delete")}
          </button>
          <hr />
          <HistoryPreference connection={connection} />
          <p className="muted small">{t("history.preferenceHint")}</p>
        </div>
      )}
    </div>
  );
}
