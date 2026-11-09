// "Move to folder…": a connection's folder, typed or picked from the folders of its layer. The
// core moves the file (and, for a repository connection, the team sees it in git).

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import type { ConnectionItem } from "../api";
import { folderOf, foldersOf, moveBlocked, moveConnection } from "../connectionMoves";
import { useApp } from "../store";

export function MoveConnection({ connection }: { connection: ConnectionItem }) {
  const { t } = useTranslation("connections");
  const connections = useApp((s) => s.connections);
  const close = () => useApp.getState().setMovingConnection(null);
  const current = folderOf(connection);
  const [folder, setFolder] = useState(current);
  const [moving, setMoving] = useState(false);
  const [error, setError] = useState<string | null>(() => moveBlocked(connection));
  const input = useRef<HTMLInputElement>(null);
  useEffect(() => {
    input.current?.focus();
    input.current?.select();
  }, []);
  const folders = foldersOf(connections, connection.personal).filter((f) => f !== current);
  const unchanged = folder.trim().replace(/^\/+|\/+$/g, "") === current;

  async function move(target: string) {
    if (moving) return;
    setMoving(true);
    setError(null);
    try {
      await moveConnection(connection, target);
      close();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setMoving(false);
    }
  }

  const listId = `move-folders-${connection.id}`;
  return (
    <div className="modal-backdrop">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label={t("move.title", { name: connection.name })}
        onKeyDown={(e) => {
          if (e.key === "Escape") {
            e.stopPropagation();
            if (!moving) close();
          }
        }}
      >
        <h2>{t("move.title", { name: connection.name })}</h2>
        <p className="muted small">{connection.personal ? t("move.personalHint") : t("move.projectHint")}</p>
        <label className="modal-field">
          <span>{t("fields.folder")}</span>
          <input
            ref={input}
            value={folder}
            list={listId}
            placeholder={t("move.noFolder")}
            spellCheck={false}
            autoComplete="off"
            disabled={moving}
            onChange={(e) => setFolder(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Enter" && !unchanged) void move(folder);
            }}
          />
        </label>
        <datalist id={listId}>
          {folders.map((f) => (
            <option key={f} value={f} />
          ))}
        </datalist>
        <p className="muted small">{t("move.folderHint")}</p>
        {error && (
          <p className="error small" role="alert">
            {error}
          </p>
        )}
        <div className="modal-actions">
          <button onClick={close} disabled={moving}>
            {t("cancel")}
          </button>
          <button className="primary" onClick={() => void move(folder)} disabled={moving || unchanged}>
            {moving ? t("move.moving") : t("move.confirm")}
          </button>
        </div>
      </div>
    </div>
  );
}
