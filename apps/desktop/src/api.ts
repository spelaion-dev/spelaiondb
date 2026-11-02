// Backend access. Everything goes through the generated types (`bindings.ts`) except
// `fetchWindow`, which receives binary Arrow that tauri-specta does not type.

import { invoke } from "@tauri-apps/api/core";
import { tableFromIPC, type Table } from "apache-arrow";
import {
  commands,
  events,
  type ColumnItem,
  type ConnectionDraftItem_Deserialize,
  type ConnectionItem_Serialize,
  type EngineItem_Serialize,
  type ConnectOutcome,
  type InteractionKind,
  type HistoryItem_Serialize,
  type ObjectItem_Serialize,
  type SchemaItem,
  type ScriptEvent_Deserialize,
  type TableDetails_Serialize,
  type TlsModeItem,
  type TunnelItem,
  type PasswordMode,
  type WorkspaceInfo_Serialize,
} from "./bindings";

export { commands, events };
export type { ColumnItem, ConnectOutcome, InteractionKind, PasswordMode, SchemaItem, TlsModeItem, TunnelItem };
export type ConnectionDraft = ConnectionDraftItem_Deserialize;
export type EngineItem = EngineItem_Serialize;
export type ConnectionItem = ConnectionItem_Serialize;
export type HistoryItem = HistoryItem_Serialize;
export type ObjectItem = ObjectItem_Serialize;
// The channel delivers the deserialization variant (optional fields).
export type ScriptEvent = ScriptEvent_Deserialize;
export type TableDetails = TableDetails_Serialize;
export type WorkspaceInfo = WorkspaceInfo_Serialize;

type Typed<T> = { status: "ok"; data: T } | { status: "error"; error: string };

/** Unwraps a command result: returns the data or throws the backend error. */
export async function call<T>(promise: Promise<Typed<T>>): Promise<T> {
  const result = await promise;
  if (result.status === "error") throw new Error(result.error);
  return result.data;
}

/** Rows [start, start + len) of a result, already converted to text by the core. */
export async function fetchWindow(result: number, start: number, len: number): Promise<Table> {
  const buffer = await invoke<ArrayBuffer>("fetch_window", { result, start, len });
  return tableFromIPC(new Uint8Array(buffer));
}
