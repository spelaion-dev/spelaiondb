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
  type TableKeyItem,
  type KeyTargetItem,
  type KeyRefItem,
  type RowReferenceItem_Serialize,
  type SelectionStatsItem_Serialize,
  type ChartAggregateItem,
  type ChartKindItem,
  type ChartLabelItem,
  type ChartPointsItem_Serialize,
  type ColumnStatsItem_Serialize,
  type DataSearchEventItem_Deserialize,
  type DataSearchLimitsItem,
  type DataSearchRequestItem,
  type DataSearchSummaryItem,
  type SchemaColumnItem,
  type SearchHitItem_Deserialize,
  type SearchTableItem,
  type TableStatusItem_Deserialize,
  type ColumnStatsOutcome_Serialize,
  type ColumnStatsTarget,
  type TopValueItem_Serialize,
  type StatStateItem,
  type RiskKind,
  type SsmDraftItem,
  type KubernetesDraftItem,
  type KubePortItem_Serialize,
  type KubeEditor_Deserialize,
  type ImportItem_Serialize,
  type ImportScan_Serialize,
  type ImportReport,
  type PendingEditItem_Deserialize,
  type PendingEditsItem_Serialize,
  type BlockItem,
  type BulkActionItem,
  type CellInputItem,
  type UpdateMethod,
  type UpdateStatus_Serialize,
  type CellEditItem_Deserialize,
  type ColumnFilterItem,
  type FilterOpItem,
  type NameStyleItem,
  type SqlTemplateItem,
  type SnippetItem,
  type TextFormat,
  type TlsModeItem,
  type TunnelItem,
  type UrlFields_Serialize,
  type PasswordMode,
  type WorkspaceInfo_Serialize,
  type ConnectionTestOutcome_Serialize,
  type TestReportItem,
  type TestStepItem,
  type McpAccessItem_Serialize,
  type McpClientItem_Serialize,
  type McpClientsItem_Serialize,
  type McpInstallPlanItem,
  type McpSettingsItem,
  type ToolOutputItem,
  type AuditEventItem_Serialize,
  type AuditSessionItem,
  type AuditStatusItem,
  type ErrorAnswerItem,
  type OnErrorItem,
  type RunOptionsItem_Deserialize,
  type ScriptPreferencesItem,
  type ScriptSummaryItem_Deserialize,
  type FileFormatItem,
  type OpenTabItem_Deserialize,
  type RecentFileItem,
  type RestoreNoteItem,
  type SkippedSetupItem,
  type RestoredTabsItem_Serialize,
  type SaveFileOutcome,
  type SqlFileItem,
} from "./bindings";

export { commands, events };
// Data search (ADR-043): its events arrive through a channel (the deserialization variant).
export type DataSearchEventItem = DataSearchEventItem_Deserialize;
export type SearchHitItem = SearchHitItem_Deserialize;
export type TableStatusItem = TableStatusItem_Deserialize;
export type { DataSearchLimitsItem, DataSearchRequestItem, DataSearchSummaryItem, SchemaColumnItem, SearchTableItem };
export type { ImportReport, KubernetesDraftItem, SsmDraftItem };
export type { BlockItem, BulkActionItem, CellInputItem };
export type EngineChangeItem = EngineChangeItem_Serialize;
export type NamePatternPreview = NamePatternPreview_Serialize;
export type { CellRange, ColumnFilterItem, ColumnItem, FilterOpItem, NameStyleItem, ConnectOutcome, FieldChangeItem, InteractionKind, PasswordMode, RiskKind, SchemaItem, SnippetItem, SqlTemplateItem, SshAuthItem, TextFormat, TlsModeItem, TunnelItem };
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
export type ImportItem = ImportItem_Serialize;
export type KubePort = KubePortItem_Serialize;
/** Which editor asks kubectl for its lists (ADR-015, ADR-050). */
export type KubeEditor = KubeEditor_Deserialize;
export type ImportScan = ImportScan_Serialize;
export type PendingEditItem = PendingEditItem_Deserialize;
export type PendingEdits = PendingEditsItem_Serialize;
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
export type { TableKeyItem, KeyTargetItem, KeyRefItem };
export type RowReferenceItem = RowReferenceItem_Serialize;
export type SelectionStatsItem = SelectionStatsItem_Serialize;
export type ChartPointsItem = ChartPointsItem_Serialize;
export type { ChartAggregateItem, ChartKindItem, ChartLabelItem };
export type ColumnStatsItem = ColumnStatsItem_Serialize;
export type ColumnStatsOutcome = ColumnStatsOutcome_Serialize;
export type TopValueItem = TopValueItem_Serialize;
export type { ColumnStatsTarget, StatStateItem };
export type IndexDetails = TableDetails["indexes"][number];
export type WorkspaceInfo = WorkspaceInfo_Serialize;
export type McpAccessItem = McpAccessItem_Serialize;
export type McpClientItem = McpClientItem_Serialize;
export type McpClientsItem = McpClientsItem_Serialize;
export type { McpInstallPlanItem, McpSettingsItem, ToolOutputItem };
export type AuditEventItem = AuditEventItem_Serialize;
export type { AuditSessionItem, AuditStatusItem };
export type UpdateStatus = UpdateStatus_Serialize;
/** How this copy of the app gets a new version: only `inApp` may offer "Install and restart". */
export type { UpdateMethod };
/** What `connect` returns. */
export type ConnectResult = ConnectOutcome_Serialize;
/** What `testConnection` returns; its steps arrive through a channel (ADR-032). */
export type ConnectionTestResult = ConnectionTestOutcome_Serialize;
export type { TestReportItem, TestStepItem };
export type { ErrorAnswerItem, OnErrorItem, ScriptPreferencesItem };
/** How one run goes (ADR-035). */
export type RunOptionsItem = RunOptionsItem_Deserialize;
export type ScriptSummaryItem = ScriptSummaryItem_Deserialize;
/** `.sql` files and the open tabs kept between runs (ADR-036). */
export type { FileFormatItem, RecentFileItem, RestoreNoteItem, SaveFileOutcome, SkippedSetupItem, SqlFileItem };
export type OpenTabItem = OpenTabItem_Deserialize;
export type RestoredTabsItem = RestoredTabsItem_Serialize;

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
