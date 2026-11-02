// "⋯" menu next to each connection in the sidebar: per-connection options. A future
// connection manager will take these over.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ConnectionItem } from "../api";
import { HistoryPreference } from "./HistoryPreference";

export function ConnectionMenu({ connection }: { connection: ConnectionItem }) {
  const { t } = useTranslation("editor");
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
          <HistoryPreference connection={connection} />
          <p className="muted small">{t("history.preferenceHint")}</p>
        </div>
      )}
    </div>
  );
}
