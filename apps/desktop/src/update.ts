// Automatic updates (ADR-021): the core side checks, downloads and verifies, and keeps the
// update's state (SDB-377): unknown, up to date, available, and while installing, how far the
// download is or why it failed. This module shows that state as the core sends it, and keeps
// only what is the page's own: a check or an action it is waiting for, and what it was told.
// Nothing is installed until the user clicks "Install and restart", which is offered only where
// the core says this copy can replace itself (on Linux, an AppImage).

import { create } from "zustand";
import { call, commands, events, type UpdateStatus } from "./api";
import { installOf, isInstalling, type UpdateRequest } from "./updateState";

export { downloadText, installOf, isInstalling, manualUpdateKey, updateProblem, type UpdateRequest } from "./updateState";

const errorText = (e: unknown) => (e instanceof Error ? e.message : String(e));

interface UpdateStore {
  /** The update's state as the core sends it (`unknown` until the first check). */
  status: UpdateStatus;
  /** Check at startup (a personal setting, on by default). */
  checkAtStartup: boolean;
  request: UpdateRequest;
  /** The notice was closed with "Later": shown again on the next start. */
  dismissed: boolean;
  notesOpen: boolean;
  start(): Promise<void>;
  check(): Promise<void>;
  install(): Promise<void>;
  /** Open the latest release's page (a fixed address chosen by the core). */
  openReleasesPage(): Promise<void>;
  setCheckAtStartup(enabled: boolean): Promise<void>;
  dismiss(): void;
  showNotes(): void;
  hideNotes(): void;
}

let listening = false;

export const useUpdate = create<UpdateStore>((set, get) => ({
  status: { kind: "unknown" },
  checkAtStartup: true,
  request: { kind: "idle" },
  dismissed: false,
  notesOpen: false,
  async start() {
    if (!listening) {
      listening = true;
      // A download's progress and its failure arrive between the command's answers.
      void events.updateChanged.listen((e) => set({ status: e.payload.status }));
    }
    try {
      const enabled = await call(commands.getUpdateCheck());
      set({ checkAtStartup: enabled });
      // A failed check at startup stays quiet: the notice is for updates, not for the network.
      if (enabled) await get().check();
      set({ request: { kind: "idle" } });
    } catch {
      set({ request: { kind: "idle" } });
    }
  },
  async check() {
    if (get().request.kind === "checking" || isInstalling(get().status)) return;
    set({ request: { kind: "checking" } });
    try {
      set({ status: await call(commands.updateCheck()), dismissed: false, request: { kind: "idle" } });
    } catch (e) {
      set({ request: { kind: "failed", action: "check", message: errorText(e) } });
    }
  },
  async install() {
    if (isInstalling(get().status)) return;
    set({ request: { kind: "idle" } });
    try {
      // On success the app restarts and this never returns.
      await call(commands.updateInstall());
    } catch (e) {
      // A failed download is the core's state (it says why, and Install tries again); a
      // refusal is the answer to this click.
      const status = await call(commands.updateStatus()).catch(() => get().status);
      set(installOf(status)?.kind === "failed"
        ? { status }
        : { status, request: { kind: "failed", action: "install", message: errorText(e) } });
    }
  },
  async openReleasesPage() {
    try {
      await call(commands.updateOpenReleasesPage());
      set({ request: { kind: "idle" } });
    } catch (e) {
      set({ request: { kind: "failed", action: "openReleasesPage", message: errorText(e) } });
    }
  },
  async setCheckAtStartup(enabled) {
    try {
      await call(commands.setUpdateCheck(enabled));
      set({ checkAtStartup: enabled, request: { kind: "idle" } });
    } catch (e) {
      set({ request: { kind: "failed", action: "setCheckAtStartup", message: errorText(e) } });
    }
  },
  dismiss() {
    set({ dismissed: true });
  },
  showNotes() {
    set({ notesOpen: true });
  },
  hideNotes() {
    set({ notesOpen: false });
  },
}));
