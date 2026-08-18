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
export interface AppConfig { data_dir: string; soft_limit: number; request_timeout_s: number; python_path: string; }

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
  getSettings: () => invoke<AppConfig>("get_settings"),
  saveSettings: (cfg: AppConfig) => invoke<void>("save_settings", { cfg }),
};
