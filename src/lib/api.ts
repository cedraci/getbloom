import { invoke } from "@tauri-apps/api/core";

export interface AssetClass { id: number; name: string; description: string; }
export interface Asset {
  id: number; asset_class_id: number; label: string; id_kind: string;
  ticker: string | null; isin: string | null; yellow_key: string;
  bdp_security: string; active: boolean;
}
export interface NewAsset {
  asset_class_id: number; label: string; id_kind: string;
  ticker: string | null; isin: string | null; yellow_key: string;
}
export interface FieldDef {
  id: number; asset_class_id: number; mnemonic: string;
  label: string; value_kind: string; active: boolean;
}
export interface View { id: number; name: string; description: string; active: boolean; }
export interface EstimateOut {
  estimated: number; today_total: number; level: "Ok" | "SoftWarn" | "HardConfirm";
}
export type RunOutcome =
  | { Completed: { run_id: number; summary: { upserted: number; issues: number } } }
  | { NeedsConfirmation: { estimated: number; today_total: number } };
export interface RunRow {
  id: number; view_id: number; kind: string; trigger_kind: string; status: string;
  started_at: string; finished_at: string | null;
  estimated_hits: number; error_summary: string | null;
}
export interface IssueRow {
  id: number; run_id: number; asset_id: number | null; field_id: number | null;
  obs_date: string | null; severity: string; code: string; detail: string;
}
export interface ScheduleRow {
  id: number; view_id: number; active: boolean; window_start: string;
  window_end: string; drawn_for: string | null; drawn_at: string | null;
  last_result: string | null;
}
export type EntityKind = "asset_class" | "asset" | "field" | "view" | "schedule";
export type DeleteMode = "retire" | "purge";
export interface DeletionImpact {
  kind: EntityKind; id: number; label: string;
  observations: number; first_obs: string | null; last_obs: string | null;
  views: number; issues: number; runs: number; children: number;
  can_retire: boolean; can_purge: boolean; blocked_reason: string | null;
}
export interface AppConfig { data_dir: string; soft_limit: number; request_timeout_s: number; python_path: string; }

export interface AssetRef { id: number; label: string; security: string; }
export interface AddRow {
  row_number: number; label: string; class: string; id_kind: string;
  ticker: string; isin: string; yellow_key: string; active: boolean;
  security: string; views: string[];
}
export interface EditRow {
  id: number; row_number: number; label: string; class: string; id_kind: string;
  ticker: string; isin: string; yellow_key: string; security: string; changed: string[];
}
export interface MembershipChange {
  id: number; label: string; added: string[]; removed: string[];
}
export interface InvalidRow { row_number: number; reason: string; }
export interface ImportPlan {
  file_hash: string; has_id_column: boolean;
  adds: AddRow[]; edits: EditRow[];
  retires: AssetRef[]; reactivations: AssetRef[];
  membership_changes: MembershipChange[]; removals: AssetRef[];
  invalid_rows: InvalidRow[];
  active_asset_count: number; requires_typed_confirmation: boolean;
}
export interface ImportResult {
  added: number; edited: number; retired: number;
  reactivated: number; membership_assets_updated: number; removed: number;
  workbook_refreshed: boolean;
}

export const api = {
  listAssetClasses: () => invoke<AssetClass[]>("list_asset_classes"),
  createAssetClass: (name: string, description: string) =>
    invoke<AssetClass>("create_asset_class", { name, description }),
  listAssets: () => invoke<Asset[]>("list_assets"),
  createAsset: (newAsset: NewAsset) => invoke<Asset>("create_asset", { new: newAsset }),
  setAssetActive: (assetId: number, active: boolean) =>
    invoke<void>("set_asset_active", { assetId, active }),
  listFields: () => invoke<FieldDef[]>("list_fields"),
  createField: (assetClassId: number, mnemonic: string, label: string, valueKind: string) =>
    invoke<FieldDef>("create_field", { assetClassId, mnemonic, label, valueKind }),
  listViews: () => invoke<View[]>("list_views"),
  createView: (name: string, description: string) =>
    invoke<View>("create_view", { name, description }),
  setViewAssets: (viewId: number, assetIds: number[]) =>
    invoke<void>("set_view_assets", { viewId, assetIds }),
  setViewFields: (viewId: number, fieldIds: number[]) =>
    invoke<void>("set_view_fields", { viewId, fieldIds }),
  getViewAssets: (viewId: number) => invoke<Asset[]>("get_view_assets", { viewId }),
  getViewFields: (viewId: number) => invoke<FieldDef[]>("get_view_fields", { viewId }),
  estimateView: (viewId: number) => invoke<EstimateOut>("estimate_view", { viewId }),
  runEodNow: (viewId: number, confirmed: boolean) =>
    invoke<RunOutcome>("run_eod_now", { viewId, confirmed }),
  runBackfillNow: (viewId: number, start: string, end: string, confirmed: boolean) =>
    invoke<RunOutcome>("run_backfill_now", { viewId, start, end, confirmed }),
  listRuns: (limit: number) => invoke<RunRow[]>("list_runs", { limit }),
  listIssues: (runId: number) => invoke<IssueRow[]>("list_issues", { runId }),
  detectViewGaps: (viewId: number) => invoke<[string, string][]>("detect_view_gaps", { viewId }),
  listSchedules: () => invoke<ScheduleRow[]>("list_schedules"),
  upsertSchedule: (viewId: number, windowStart: string, windowEnd: string, active: boolean) =>
    invoke<void>("upsert_schedule", { viewId, windowStart, windowEnd, active }),
  describeDeletion: (kind: EntityKind, id: number) =>
    invoke<DeletionImpact>("describe_deletion", { kind, id }),
  deleteAsset: (id: number, mode: DeleteMode) => invoke<void>("delete_asset", { id, mode }),
  deleteField: (id: number, mode: DeleteMode) => invoke<void>("delete_field", { id, mode }),
  deleteView: (id: number, mode: DeleteMode) => invoke<void>("delete_view", { id, mode }),
  deleteAssetClass: (id: number) => invoke<void>("delete_asset_class", { id }),
  deleteSchedule: (id: number) => invoke<void>("delete_schedule", { id }),
  getSettings: () => invoke<AppConfig>("get_settings"),
  saveSettings: (cfg: AppConfig) => invoke<void>("save_settings", { cfg }),
  exportAssetsXlsx: (path: string) => invoke<void>("export_assets_xlsx", { path }),
  previewAssetsImport: (path: string) => invoke<ImportPlan>("preview_assets_import", { path }),
  applyAssetsImport: (path: string, fileHash: string,
                      removalModes: [number, DeleteMode][],
                      confirmedRemovalCount: number | null) =>
    invoke<ImportResult>("apply_assets_import",
      { path, fileHash, removalModes, confirmedRemovalCount }),
};
