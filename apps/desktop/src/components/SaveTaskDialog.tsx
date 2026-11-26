// "Save as task…" (ADR-089 point 10): the export, import, backup and SQL file dialogs save the
// request they would run, as they would send it, under a name and a folder. The core checks it
// (the checks of a run by hand, and that it holds no secret) and keeps it; a name another task
// has is asked about before that task is replaced. Exports name the folder their files go to
// here, since a task never writes over a file and the dialog's save dialog picked one file.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { create } from "zustand";
import { call, commands } from "../api";
import type { TaskRequestItem } from "../bindings";
import { chooseFolder } from "../dialogs";
import { errorText } from "../dialogState";
import { useApp } from "../store";

/** What a dialog asks to save. `request` gets the chosen output folder when `needsFolder`. */
export interface SaveTaskRequest {
  /** The dialog's session: the task takes its connection. */
  session: string;
  /** The name offered. */
  name: string;
  /** The request with an output folder put in, for an export. */
  request(folder: string): TaskRequestItem;
  /** An export: the files' folder is chosen here. */
  needsFolder: boolean;
}

export const useSaveTask = create<{ request: SaveTaskRequest | null; open(request: SaveTaskRequest): void; close(): void }>((set) => ({
  request: null,
  open: (request) => set({ request }),
  close: () => set({ request: null }),
}));

type Phase =
  | { kind: "editing" }
  | { kind: "saving" }
  | { kind: "taken"; id: number }
  | { kind: "failed"; message: string }
  | { kind: "saved"; name: string };

export function SaveTaskDialog() {
  const request = useSaveTask((s) => s.request);
  if (!request) return null;
  return <SaveTaskForm request={request} />;
}

function SaveTaskForm({ request }: { request: SaveTaskRequest }) {
  const { t } = useTranslation("shell");
  const close = useSaveTask((s) => s.close);
  const [name, setName] = useState(request.name);
  const [folder, setFolder] = useState("");
  const [output, setOutput] = useState("");
  const [phase, setPhase] = useState<Phase>({ kind: "editing" });
  const first = useRef<HTMLInputElement>(null);
  const busy = phase.kind === "saving";
  useEffect(() => {
    first.current?.focus();
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape" && !busy) {
        e.stopPropagation();
        close();
      }
    };
    // Captured: the dialog it opens over keeps its own Escape for itself.
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [busy, close]);

  async function save(replace: number | null) {
    setPhase({ kind: "saving" });
    try {
      const outcome = await call(commands.saveTask(request.session, name, folder, replace, request.request(output.trim())));
      switch (outcome.kind) {
        case "saved":
          setPhase({ kind: "saved", name: name.trim() });
          break;
        case "nameTaken":
          setPhase({ kind: "taken", id: outcome.id ?? 0 });
          break;
        default: {
          const unexpected: never = outcome;
          return unexpected;
        }
      }
    } catch (e) {
      setPhase({ kind: "failed", message: errorText(e) });
    }
  }

  const ready = name.trim() !== "" && (!request.needsFolder || output.trim() !== "");

  return (
    <div className="modal-backdrop save-task-backdrop">
      <div className="modal save-task-dialog" role="dialog" aria-modal="true" aria-labelledby="save-task-title">
        <h2 id="save-task-title">{t("tasks.saveTitle")}</h2>
        {phase.kind === "saved" ? (
          <>
            <p role="status">{t("tasks.savedNotice", { name: phase.name })}</p>
            <div className="modal-actions">
              <button
                onClick={() => {
                  close();
                  useApp.getState().setSidebarSection("tasks");
                }}
              >
                {t("tasks.openTasks")}
              </button>
              <button className="primary" onClick={close}>
                {t("tasks.done")}
              </button>
            </div>
          </>
        ) : (
          <>
            <p className="muted">{t("tasks.saveIntro")}</p>
            <label className="field">
              {t("tasks.name")}
              <input ref={first} name="task-name" value={name} disabled={busy} onChange={(e) => {
                  setName(e.target.value);
                  // Replacing names the task that had the name asked about, not a new one.
                  if (phase.kind === "taken") setPhase({ kind: "editing" });
                }} onKeyDown={(e) => e.key === "Enter" && ready && void save(null)} />
            </label>
            <label className="field">
              {t("tasks.folder")}
              <input name="task-folder" value={folder} placeholder={t("tasks.folderPlaceholder")} disabled={busy} onChange={(e) => setFolder(e.target.value)} />
            </label>
            {request.needsFolder && (
              <div className="field">
                <label htmlFor="task-output">{t("tasks.outputFolder")}</label>
                <div className="tasks-path-field">
                  <input id="task-output" name="task-output" value={output} spellCheck={false} disabled={busy} onChange={(e) => setOutput(e.target.value)} />
                  <button
                    disabled={busy}
                    onClick={() =>
                      void chooseFolder(t("tasks.outputFolder"), output || null).then((chosen) => {
                        if (chosen) setOutput(chosen);
                      })
                    }
                  >
                    {t("tasks.choose")}
                  </button>
                </div>
                <p className="muted small">{t("tasks.outputHint")}</p>
              </div>
            )}
            <p className="muted small">{t("tasks.noSecrets")}</p>
            {phase.kind === "taken" && (
              <p className="warning" role="alert">
                {t("tasks.nameTaken", { name: name.trim() })}
              </p>
            )}
            {phase.kind === "failed" && (
              <p className="error" role="alert">
                {t("tasks.couldNotSave", { reason: phase.message })}
              </p>
            )}
            <div className="modal-actions">
              <button disabled={busy} onClick={close}>
                {t("tasks.cancel")}
              </button>
              {phase.kind === "taken" ? (
                <button className="danger" onClick={() => void save(phase.id)}>
                  {t("tasks.replace")}
                </button>
              ) : (
                <button className="primary" disabled={busy || !ready} onClick={() => void save(null)}>
                  {busy ? t("tasks.saving") : t("tasks.save")}
                </button>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
