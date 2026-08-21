import { invoke } from "@tauri-apps/api/core";

export interface AssetClass { id: number; name: string; description: string; }
export interface FieldDef {
  id: number; asset_class_id: number; mnemonic: string;
  label: string; value_kind: string;
  bbg_ftype: string | null; bbg_datatype: string | null; entitlement_note: string;
  active: boolean;
  qc_nonpositive: boolean; qc_outlier_pct: number | null; qc_stale_days: number | null;
}
export interface View { id: number; name: string; description: string; active: boolean; }
export interface EstimateOut {
  estimated: number; today_total: number; level: "Ok" | "SoftWarn" | "HardConfirm";
}
export type RunOutcome =
  | { Completed: { run_id: number;
                    summary: { inserted: number; superseded: number;
                               unchanged: number; issues: number };
                    corp_actions?: ViewRefreshCorpActionsSummary | null;
                    quality_findings: number } }
  | { NeedsConfirmation: { estimated: number; today_total: number } };
export interface RunRow {
  id: number; view_id: number; kind: string; trigger_kind: string; status: string;
  started_at: string; finished_at: string | null;
  estimated_hits: number; error_summary: string | null;
}
export interface IssueRow {
  id: number; run_id: number | null; instrument_id: number | null;
  field_id: number | null;
  obs_date: string | null; severity: string; code: string; detail: string;
  label: string | null;
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
  // True when `observations`/`issues` above survive a purge instead of
  // being deleted by it -- currently only for kind "asset" (a book entry):
  // purging one removes it from the book and from its views, never the
  // underlying instrument, its aliases, or its recorded history.
  purge_keeps_history: boolean;
}
export interface AppConfig { data_dir: string; soft_limit: number; request_timeout_s: number; python_path: string; }

export interface BookEntry {
  instrument_id: number; asset_class_id: number; label: string; active: boolean;
  note: string;
  /// Derived from today's alias; null when the instrument has no security
  /// string valid today (a delisted instrument, for instance).
  security: string | null;
}

export interface AddToBook {
  raw: string; yellow_key: string; asset_class_id: number; label: string;
  hints: { exchange?: string | null; country?: string | null;
           currency?: string | null; asset_class?: string | null };
}
export type AddOutcome =
  | { Added: BookEntry }
  | { NeedsReview: { review_id: number } }
  | "NotFound";
export type SearchOrigin = "book" | "instrument" | "candidate";
export interface SearchHit {
  origin: SearchOrigin; security: string | null; display: string;
  description: string; instrument_id: number | null; similarity: number;
}
export interface BloombergSearch {
  hits: SearchHit[]; estimated_hits: number; cached: number;
}
// resolution/engine.rs's `PendingReview.candidates` is a bare
// `serde_json::Value` in Rust, not a typed column -- and `resolution_decision
// .candidates` is written by three different code paths that each shape it
// differently (see engine.rs `resolve` and `resolve_review`):
//
// 1. Scored candidates -- what both review-opening paths (a local alias
//    matching more than one live instrument, and an ambiguous Bloomberg
//    search) actually write today via `serde_json::to_value(&Vec<Scored>)`.
//    This is the shape a *pending* review's candidates have as the code
//    stands, and the only one with enough structure for "pick this one"
//    buttons.
// 2. The local single-match note `{"matched": <id_type>, "bloomberg_calls":
//    0}`, written when a bare identifier resolves to exactly one instrument
//    (`Resolution::Bound`, no review opened).
// 3. The manual-resolution note written by `resolve_review` itself --
//    `{chosen_security, bloomberg_fallback, review_id, source_decision_id,
//    original_candidates}` -- for the *new* decision row a human's choice
//    creates, not the review being closed.
//
// Shapes 2 and 3 do not currently reach a *pending* review (neither writes a
// `resolution_review` row), but nothing enforces that from the type system on
// either side -- `candidates` is `Value` in Rust precisely because it isn't
// one shape. Declaring only shape 1 here would be a lie the compiler cannot
// catch. Render every shape defensively; never assume which one arrived.
export interface ScoredCandidate {
  candidate: {
    security: string; description: string; exchange: string | null;
    country?: string | null; currency?: string | null;
    asset_class?: string | null; figi?: string | null;
  };
  score: number; disqualified: boolean; reasons: string[];
  /// Present ONLY on a local-ambiguity entry: the existing instrument this
  /// row stands for. It is what the review screen hands back, because the
  /// `security` field above may be `instrument #42` -- a placeholder for an
  /// instrument with no current security string, which Bloomberg cannot
  /// resolve and which must never become an alias value.
  instrument_id?: number | null;
}
/// A bare identifier already matches more than one instrument already in the
/// database. No Bloomberg call can resolve that, so none was made -- and none
/// is needed to close it: the user points at one of the instruments and the
/// review is re-pointed locally, for free.
export interface LocalAmbiguityNote {
  local_ambiguity: true;
  matched: string; bloomberg_calls: number;
  candidates: ScoredCandidate[];
}
export interface ManualResolutionNote {
  chosen_security: string; bloomberg_fallback: boolean;
  review_id: number; source_decision_id: number; original_candidates: unknown;
}
export type ReviewCandidates =
  | ScoredCandidate[] | LocalAmbiguityNote | ManualResolutionNote | unknown;

/// What `history::ingest` did with one anchored response.
export interface HistoryOutcome {
  aliases_added: number;
  links_proposed: number[];
}

/// A stretch of weekdays ONE instrument in a view is missing. Per instrument,
/// not per view: one member reporting on a date says nothing about the others.
export interface GapRow {
  instrument_id: number; label: string; start: string; end: string;
}

export interface PendingReview {
  review_id: number; decision_id: number; raw_input: string; normalized: string;
  candidates: ReviewCandidates;
  bbg_response: unknown | null; opened_at: string;
}

/// Bloomberg exposes no successor field, so every `instrument_link` row is
/// inferred (see `commands::LinkProposal`). One with `confirmed_by IS NULL`
/// is a proposal no query may follow until a human confirms it.
export interface LinkProposal {
  id: number; predecessor_id: number; successor_id: number;
  predecessor_label: string | null; successor_label: string | null;
  link_type: string; effective_date: string; evidence: unknown;
  /// P6: Bloomberg's asserted ratio (acquirer sh. per target sh.), if any.
  exchange_ratio: number | null;
}
/// P6: what one on-demand lifecycle check did (the same flow runs
/// automatically after every completed run).
export interface LifecycleSummary {
  checked: number; dead: number;
  links_proposed: number; links_confirmed: number; issues: number;
}
export interface AliasRow {
  id: number; instrument_id: number; id_type: string; value: string;
  exch_code: string | null; valid_from: string; valid_to: string; source: string;
  bbg_action_id: string | null; anchoring_identifier: string | null;
}
export interface AttrRow {
  id: number; instrument_id: number; attr: string; value: string;
  valid_from: string; valid_to: string; source: string;
}
export interface RefreshCorpActionsSummary {
  inserted: number; amended: number; withdrawn: number;
  unchanged: number; unparsed: number;
}
export interface ObsRow {
  id: number; obs_date: string; value_num: number | null;
  value_text: string | null; basis_note: string | null; layer: string;
  run_id: number; system_from: string; current: boolean;
}
export interface CorpActionFull {
  id: number; source_field: string; natural_key: string;
  event_date: string | null; amount: number | null;
  operator: number | null; flag: number | null;
  dvd_type: string | null; frequency: string | null;
  declared_date: string | null; record_date: string | null;
  pay_date: string | null; amount_status: string | null;
  payload: Record<string, unknown>; system_from: string; current: boolean;
}
export interface ViewRefreshCorpActionsSummary {
  instruments: number; skipped: number; failed: number; not_applicable: number;
  inserted: number; amended: number;
  withdrawn: number; unchanged: number; unparsed: number;
}
export type AdjustModeStr = "raw" | "splits" | "all";
export interface StitchRow {
  obs_date: string; value: number; source_instrument_id: number;
}
export interface SegmentInfo {
  instrument_id: number; label: string | null; from: string | null;
  to: string | null; link_type: string | null; ratio: number | null;
  note: string | null;
}
export interface StitchedSeries {
  rows: StitchRow[]; segments: SegmentInfo[]; stopped: string | null;
}
export interface AdjRow { obs_date: string; raw: number; adjusted: number; }
export interface AdjSeries {
  rows: AdjRow[]; factors_used: number; unusable_factors: number;
}
export interface CorpActionRow {
  id: number; source_field: string; event_date: string | null;
  amount: number | null; operator: number | null; flag: number | null;
  dvd_type: string | null; amount_status: string | null;
  pay_date: string | null; fully_parsed_key: boolean;
}

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
  reviews_opened: number; not_found: number;
  workbook_refreshed: boolean;
}

export const api = {
  listAssetClasses: () => invoke<AssetClass[]>("list_asset_classes"),
  createAssetClass: (name: string, description: string) =>
    invoke<AssetClass>("create_asset_class", { name, description }),
  listBook: () => invoke<BookEntry[]>("list_book"),
  addToBook: (req: AddToBook) => invoke<AddOutcome>("add_to_book", { req }),
  setBookActive: (instrumentId: number, active: boolean) =>
    invoke<void>("set_book_active", { instrumentId, active }),
  searchLocal: (query: string, limit = 12) =>
    invoke<SearchHit[]>("search_local", { query, limit }),
  searchBloomberg: (query: string, yellowKey: string) =>
    invoke<BloombergSearch>("search_bloomberg", { query, yellowKey }),
  listPendingReviews: () => invoke<PendingReview[]>("list_pending_reviews"),
  resolveReview: (reviewId: number, chosenSecurity: string) =>
    invoke<number>("resolve_review", { reviewId, chosenSecurity }),
  /// Closes a LOCALLY ambiguous review by pointing at an instrument that
  /// already exists. Zero Bloomberg calls, by construction.
  resolveReviewLocal: (reviewId: number, instrumentId: number) =>
    invoke<number>("resolve_review_local", { reviewId, instrumentId }),
  rejectReview: (reviewId: number, note: string) =>
    invoke<void>("reject_review", { reviewId, note }),
  /// One anchored HISTORICAL_IDS_TIME_RANGE request. The anchor is the
  /// identifier the chain STARTED from and only the user can know it -- see
  /// `commands::ingest_identifier_history`.
  ingestIdentifierHistory: (instrumentId: number, anchor: string, rangeStart: string) =>
    invoke<HistoryOutcome>("ingest_identifier_history",
                           { instrumentId, anchor, rangeStart }),
  listLinkProposals: () => invoke<LinkProposal[]>("list_link_proposals"),
  confirmLink: (linkId: number) => invoke<void>("confirm_link", { linkId }),
  runLifecycleCheck: () => invoke<LifecycleSummary>("run_lifecycle_check"),
  listStandaloneIssues: () => invoke<IssueRow[]>("list_standalone_issues"),
  instrumentAliases: (instrumentId: number) =>
    invoke<AliasRow[]>("instrument_aliases", { instrumentId }),
  instrumentAttrs: (instrumentId: number) =>
    invoke<AttrRow[]>("instrument_attrs", { instrumentId }),
  refreshCorpActions: (instrumentId: number) =>
    invoke<RefreshCorpActionsSummary>("refresh_corp_actions", { instrumentId }),
  refreshViewCorpActions: (viewId: number) =>
    invoke<ViewRefreshCorpActionsSummary>("refresh_view_corp_actions", { viewId }),
  listObservations: (instrumentId: number, fieldId: number,
                     includeSuperseded: boolean, limit: number) =>
    invoke<ObsRow[]>("list_observations",
                     { instrumentId, fieldId, includeSuperseded, limit }),
  listCorpActionsFull: (instrumentId: number, includeSuperseded: boolean) =>
    invoke<CorpActionFull[]>("list_corp_actions_full",
                             { instrumentId, includeSuperseded }),
  exportObservationsCsv: (instrumentId: number, fieldId: number, path: string) =>
    invoke<number>("export_observations_csv", { instrumentId, fieldId, path }),
  exportCorpActionsCsv: (instrumentId: number, path: string) =>
    invoke<number>("export_corp_actions_csv", { instrumentId, path }),
  listAdjusted: (instrumentId: number, fieldId: number, mode: AdjustModeStr,
                 limit: number) =>
    invoke<AdjSeries>("list_adjusted", { instrumentId, fieldId, mode, limit }),
  exportAdjustedCsv: (instrumentId: number, fieldId: number, mode: AdjustModeStr,
                      path: string) =>
    invoke<number>("export_adjusted_csv", { instrumentId, fieldId, mode, path }),
  listStitched: (instrumentId: number, fieldId: number, mode: AdjustModeStr,
                 limit: number) =>
    invoke<StitchedSeries>("list_stitched", { instrumentId, fieldId, mode, limit }),
  hasConfirmedPredecessors: (instrumentId: number) =>
    invoke<boolean>("has_confirmed_predecessors", { instrumentId }),
  exportStitchedCsv: (instrumentId: number, fieldId: number, mode: AdjustModeStr,
                      path: string) =>
    invoke<number>("export_stitched_csv", { instrumentId, fieldId, mode, path }),
  listCorpActions: (instrumentId: number) =>
    invoke<CorpActionRow[]>("list_corp_actions", { instrumentId }),
  listFields: () => invoke<FieldDef[]>("list_fields"),
  createField: (assetClassId: number, mnemonic: string, label: string, valueKind: string,
                bbgFtype: string | null = null, bbgDatatype: string | null = null,
                entitlementNote: string | null = null,
                qcNonpositive: boolean = false, qcOutlierPct: number | null = null,
                qcStaleDays: number | null = null) =>
    invoke<FieldDef>("create_field",
      { assetClassId, mnemonic, label, valueKind, bbgFtype, bbgDatatype, entitlementNote,
        qcNonpositive, qcOutlierPct, qcStaleDays }),
  listViews: () => invoke<View[]>("list_views"),
  createView: (name: string, description: string) =>
    invoke<View>("create_view", { name, description }),
  setViewInstruments: (viewId: number, instrumentIds: number[]) =>
    invoke<void>("set_view_instruments", { viewId, instrumentIds }),
  setViewFields: (viewId: number, fieldIds: number[]) =>
    invoke<void>("set_view_fields", { viewId, fieldIds }),
  getViewInstruments: (viewId: number) =>
    invoke<BookEntry[]>("get_view_instruments", { viewId }),
  getViewFields: (viewId: number) => invoke<FieldDef[]>("get_view_fields", { viewId }),
  estimateView: (viewId: number) => invoke<EstimateOut>("estimate_view", { viewId }),
  runEodNow: (viewId: number, confirmed: boolean) =>
    invoke<RunOutcome>("run_eod_now", { viewId, confirmed }),
  runBackfillNow: (viewId: number, start: string, end: string, confirmed: boolean,
                   instrumentIds: number[] | null = null) =>
    invoke<RunOutcome>("run_backfill_now", { viewId, start, end, instrumentIds, confirmed }),
  listRuns: (limit: number) => invoke<RunRow[]>("list_runs", { limit }),
  listIssues: (runId: number) => invoke<IssueRow[]>("list_issues", { runId }),
  detectViewGaps: (viewId: number) => invoke<GapRow[]>("detect_view_gaps", { viewId }),
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
