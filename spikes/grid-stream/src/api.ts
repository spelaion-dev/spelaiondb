import { Channel, invoke } from "@tauri-apps/api/core";
import { tableFromIPC, type Table } from "apache-arrow";

export interface OpenedResult {
  id: number;
  columns: string[];
  rows: number;
  elapsedMs: number;
}

export type LoadEvent =
  | { kind: "started"; id: number; columns: string[] }
  | { kind: "progress"; rows: number; elapsedMs: number; firstBatchMs: number }
  | { kind: "done"; rows: number; elapsedMs: number; firstBatchMs: number }
  | { kind: "failed"; message: string };

export interface MemoryStats {
  treeMb: number;
  ownMb: number;
  processes: number;
}

export interface SystemInfo {
  os: string;
  arch: string;
  webview: string;
  autorun: boolean;
  rows: number;
  pg: string | null;
  pgSql: string;
}

export const openSynthetic = (rows: number) => invoke<OpenedResult>("open_synthetic", { rows });

export function openPostgres(conn: string, sql: string, onEvent: (e: LoadEvent) => void) {
  const channel = new Channel<LoadEvent>();
  channel.onmessage = onEvent;
  return invoke<void>("open_postgres", { conn, sql, onEvent: channel });
}

/** Requests rows [start, start + len) from the backend as binary Arrow IPC (no JSON). */
export async function fetchWindow(id: number, start: number, len: number): Promise<Table> {
  const buf = await invoke<ArrayBuffer>("fetch_window", { id, start, len });
  return tableFromIPC(new Uint8Array(buf));
}

export const memoryStats = () => invoke<MemoryStats>("memory_stats");
export const systemInfo = () => invoke<SystemInfo>("system_info");
export const saveReport = (report: unknown) => invoke<string>("save_report", { report });
export const exitApp = () => invoke<void>("exit_app");
