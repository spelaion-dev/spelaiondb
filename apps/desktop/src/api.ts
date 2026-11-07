// Backend access. Everything goes through the generated types (`bindings.ts`) except
// `fetchWindow`, which receives binary Arrow that tauri-specta does not type.

import { invoke } from "@tauri-apps/api/core";
import { tableFromIPC, type Table } from "apache-arrow";
import {
  commands,
  events,
  type BookmarkItem,
  type CellRange,
  type ColumnItem,
  type ConnectionDraftItem_Deserialize,
  type ConnectionItem_Serialize,
  type ConnectionTypeDraft_Deserialize,
  type ConnectionTypeItem_Serialize,
  type TransactionStatusItem_Serialize,
  type EngineItem_Serialize,
  type EngineChangeItem_Serialize,
  type NamePatternPreview_Serialize,
  type HopDraftItem_Deserialize,
  type SshAuthItem,
  type TunnelDraftItem_Deserialize,
  type ProxyDraftItem_Deserialize,
  type NetworkProfileItem_Serialize,
  type ConnectOutcome,
  type CreateKindItem,
  type ConnectOutcome_Serialize,
  type FieldChangeItem,
  type InteractionKind,
  type HistoryItem_Serialize,
  type ObjectActionsItem,
  type ObjectChangeItem,
  type ObjectTargetItem,
  type ObjectItem_Serialize,
  type ObjectPathItem,
  type ExplorerPreferencesItem,
  type GridPreferencesItem,
  type ExplorerFilterItem,
  type NameFilterItem,
  type NewIndexKindItem,
  type FilterKindItem,
  type FoundObjectItem_Serialize,
  type FilterActionItem,
  type SchemaObjectItem_Serialize,
  type SchemaObjectKindItem,
  type ServerObjectItem_Serialize,
  type ServerObjectKindItem,
  type SchemaItem,
  type ScriptEvent_Deserialize,
  type TableDetails_Serialize,
  type RiskKind,
  type KubernetesDraftItem,
  type KubePortItem_Serialize,
  type DbeaverItem_Serialize,
  type DbeaverScan_Serialize,
  type ImportReport,
  type RowChangeItem_Deserialize,
  type UpdateStatus_Serialize,
  type CellEditItem_Deserialize,
  type ColumnFilterItem,
  type FilterOpItem,
  type SqlTemplateItem,
  type SnippetItem,
  type TextFormat,
  type TlsModeItem,
  type TunnelItem,
  type UrlFields_Serialize,
  type PasswordMode,
  type WorkspaceInfo_Serialize,
} from "./bindings";

export { commands, events };
export type { ImportReport, KubernetesDraftItem };
export type EngineChangeItem = EngineChangeItem_Serialize;
export type NamePatternPreview = NamePatternPreview_Serialize;
export type { CellRange, ColumnFilterItem, ColumnItem, FilterOpItem, ConnectOutcome, FieldChangeItem, InteractionKind, PasswordMode, RiskKind, SchemaItem, SnippetItem, SqlTemplateItem, SshAuthItem, TextFormat, TlsModeItem, TunnelItem };
export type HopDraft = HopDraftItem_Deserialize;
export type TunnelDraft = TunnelDraftItem_Deserialize;
export type ProxyDraft = ProxyDraftItem_Deserialize;
export type NetworkProfileItem = NetworkProfileItem_Serialize;
export type ConnectionDraft = ConnectionDraftItem_Deserialize;
export type EngineItem = EngineItem_Serialize;
export type ConnectionItem = ConnectionItem_Serialize;
export type ConnectionTypeItem = ConnectionTypeItem_Serialize;
export type TransactionStatusItem = TransactionStatusItem_Serialize;
export type ConnectionTypeDraft = ConnectionTypeDraft_Deserialize;
export type DbeaverItem = DbeaverItem_Serialize;
export type KubePort = KubePortItem_Serialize;
export type DbeaverScan = DbeaverScan_Serialize;
export type RowChangeItem = RowChangeItem_Deserialize;
export type CellEditItem = CellEditItem_Deserialize;
export type UrlFields = UrlFields_Serialize;
export type HistoryItem = HistoryItem_Serialize;
export type ObjectItem = ObjectItem_Serialize;
export type ExplorerPreferences = ExplorerPreferencesItem;
export type GridPreferences = GridPreferencesItem;
export type ExplorerFilter = ExplorerFilterItem;
export type NameFilter = NameFilterItem;
export type { BookmarkItem, CreateKindItem, FilterActionItem, FilterKindItem, NewIndexKindItem, ObjectActionsItem, ObjectChangeItem, ObjectPathItem, ObjectTargetItem };
export type SchemaObjectItem = SchemaObjectItem_Serialize;
export type FoundObjectItem = FoundObjectItem_Serialize;
export type ServerObjectItem = ServerObjectItem_Serialize;
export type { SchemaObjectKindItem, ServerObjectKindItem };
// The channel delivers the deserialization variant (optional fields).
export type ScriptEvent = ScriptEvent_Deserialize;
export type TableDetails = TableDetails_Serialize;
export type IndexDetails = TableDetails["indexes"][number];
export type WorkspaceInfo = WorkspaceInfo_Serialize;
export type UpdateStatus = UpdateStatus_Serialize;
/** What `connect` and `testConnection` return. */
export type ConnectResult = ConnectOutcome_Serialize;

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
