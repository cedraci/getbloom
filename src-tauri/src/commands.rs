use crate::error::AppError;
use crate::instrument::search;
use crate::master_fetch;
use crate::orchestrator::{self, PipelineConfig, RunOutcome};
use crate::resolution::engine;
use crate::scheduler::previous_weekday;
use crate::{book, budget, bulk, deletion, fields, registry, scheduler, views};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::path::PathBuf;
use tauri::State;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub data_dir: String,
    pub soft_limit: i64,
    pub request_timeout_s: u32,
    /// Interpreter used to run the BLPAPI sidecar. Bare "python" resolves via
    /// PATH; set an absolute path when several interpreters are installed and
    /// only one has the `blpapi` package.
    pub python_path: String,
    /// P10 task 7. `#[serde(default)]` so an old config.json (written before
    /// this field existed) still parses -- None on every new field. Startup
    /// precedence (lib.rs): config.json -> BLOOM_DATABASE_URL env -> hardcoded
    /// default; deliberately UI-first, so the user who edits the UI sees an
    /// effect even with the env var set. Takes effect on restart.
    #[serde(default)]
    pub database_url: Option<String>,
    /// Remote Bloomberg Terminal host/port. None = the sidecar's own
    /// localhost:8194 default (zero Python changes -- see fetch::SidecarPayload).
    #[serde(default)]
    pub blp_host: Option<String>,
    #[serde(default)]
    pub blp_port: Option<u16>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { data_dir: "C:\\bloomdata".into(),
               soft_limit: budget::DEFAULT_SOFT_LIMIT,
               request_timeout_s: 120,   // BLPAPI answers in seconds, not the minutes Excel needed
               python_path: "python".into(),
               database_url: None,
               blp_host: None,
               blp_port: None }
    }
}

pub struct AppState {
    pub pool: PgPool,
    pub cfg: tokio::sync::RwLock<AppConfig>,
}

pub fn script_path() -> PathBuf {
    // scripts/blp_fetch.py ships next to the executable (bundled as a Tauri
    // resource); in dev it resolves relative to src-tauri/.
    let exe_dir = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let bundled = exe_dir.join("scripts").join("blp_fetch.py");
    if bundled.exists() { bundled } else { PathBuf::from("scripts/blp_fetch.py") }
}

pub async fn pipeline_cfg(state: &AppState) -> PipelineConfig {
    let c = state.cfg.read().await.clone();
    PipelineConfig {
        data_dir: PathBuf::from(c.data_dir),
        python_path: PathBuf::from(c.python_path),
        script_path: script_path(),
        request_timeout_s: c.request_timeout_s,
        soft_limit: c.soft_limit,
        blp_host: c.blp_host,
        blp_port: c.blp_port,
    }
}

#[derive(Debug, Serialize)]
pub struct EstimateOut {
    pub estimated: i64,
    pub today_total: i64,
    pub level: budget::BudgetLevel,
}

#[derive(Debug, Serialize)]
pub struct BudgetToday {
    pub hits: i64,
    pub soft_limit: i64,
}

// ---------------------------------------------------------------------------
// Asset classes
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_asset_classes(state: State<'_, AppState>)
    -> Result<Vec<registry::AssetClass>, AppError> {
    registry::list_asset_classes(&state.pool).await
}

#[tauri::command]
pub async fn create_asset_class(state: State<'_, AppState>, name: String, description: String)
    -> Result<registry::AssetClass, AppError> {
    registry::create_asset_class(&state.pool, &name, &description).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn update_asset_class_capabilities(state: State<'_, AppState>, id: i64,
    corp_actions_capable: bool, ma_capable: bool, adjustment_style: String,
    qc_stale_days_default: Option<i32>, default_cadence: String,
    cadence_grace_days: i32, identity_sweep: String) -> Result<(), AppError>
{
    registry::update_asset_class_capabilities(&state.pool, id, corp_actions_capable,
        ma_capable, &adjustment_style, qc_stale_days_default,
        &default_cadence, cadence_grace_days, &identity_sweep).await
}

// ---------------------------------------------------------------------------
// Book
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_book(state: State<'_, AppState>)
    -> Result<Vec<book::BookEntry>, AppError> {
    book::list(&state.pool).await
}

#[tauri::command]
pub async fn add_to_book(state: State<'_, AppState>, req: book::AddToBook)
    -> Result<book::AddOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    book::add(&state.pool, &fetcher, &req, "user").await
}

#[tauri::command]
pub async fn set_book_active(state: State<'_, AppState>, instrument_id: i64,
                             active: bool) -> Result<(), AppError> {
    book::set_active(&state.pool, instrument_id, active).await
}

// ---------------------------------------------------------------------------
// Resolution review
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_pending_reviews(state: State<'_, AppState>)
    -> Result<Vec<engine::PendingReview>, AppError> {
    engine::pending_reviews(&state.pool).await
}

#[tauri::command]
pub async fn resolve_review(state: State<'_, AppState>, review_id: i64,
                            chosen_security: String) -> Result<i64, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    let as_of = chrono::Local::now().date_naive();
    engine::resolve_review(&state.pool, &fetcher, review_id, &chosen_security, "user", as_of).await
}

/// A locally ambiguous review, resolved by pointing at one of the existing
/// instruments the identifier already matched. Costs zero Bloomberg calls --
/// there is nothing for Bloomberg to answer about an ambiguity that is
/// entirely local (spec §7). Deliberately a separate command from
/// `resolve_review`, which always calls out.
#[tauri::command]
pub async fn resolve_review_local(state: State<'_, AppState>, review_id: i64,
                                  instrument_id: i64) -> Result<i64, AppError> {
    engine::resolve_review_local(&state.pool, review_id, instrument_id, "user").await
}

#[tauri::command]
pub async fn reject_review(state: State<'_, AppState>, review_id: i64, note: String)
    -> Result<(), AppError> {
    engine::reject_review(&state.pool, review_id, &note).await
}

/// Identifier history (`HISTORICAL_IDS_TIME_RANGE`), as an explicit user
/// action and nothing else.
///
/// It used to fire automatically on every successful resolution. P0 §6.5
/// measured why it cannot: `HISTORICAL_STARTING_IDENTIFIER` names the
/// identifier the chain STARTED from, and resolution only ever knows the one
/// it ENDED at. Anchored on the resolved current security, the request
/// returned a well-formed chain belonging to a different company -- for
/// `META US Equity`, the Roundhill Ball Metaverse ETF's rename to `METV`. It
/// cannot discover a rename; it can only confirm one somebody already knows
/// about. So the anchor comes from the user, who is the only party that can
/// know it.
///
/// Costs one bulk `ReferenceDataRequest`, charged to the ledger at the wire
/// seam under purpose `resolve_history`.
#[tauri::command]
pub async fn ingest_identifier_history(state: State<'_, AppState>, instrument_id: i64,
                                       anchor: String, range_start: String)
    -> Result<crate::instrument::history::HistoryOutcome, AppError>
{
    let start: chrono::NaiveDate = range_start.parse().map_err(|_| {
        AppError::Validation(format!("range start {range_start:?} is not a date (YYYY-MM-DD)"))
    })?;
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    crate::instrument::history::ingest(&state.pool, &fetcher, instrument_id,
                                       &anchor, start).await
}

/// Bloomberg exposes no successor field (P0 7.2), so every `instrument_link`
/// row is inferred, not asserted. A row with `confirmed_by IS NULL` is a
/// proposal: no query may follow it (see `store::confirmed_successors`) until
/// a human agrees. `predecessor_label`/`successor_label` come from
/// `book_entry` and are `None` for an instrument never added to the book --
/// the proposal is still shown, by instrument id, rather than dropped.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LinkProposal {
    pub id: i64,
    pub predecessor_id: i64,
    pub successor_id: i64,
    pub predecessor_label: Option<String>,
    pub successor_label: Option<String>,
    pub link_type: String,
    pub effective_date: chrono::NaiveDate,
    pub evidence: serde_json::Value,
    /// P6: Bloomberg's asserted ratio (acquirer sh. per target sh.), when
    /// the lifecycle flow found one. Shown so a reviewer confirming a
    /// merger sees the terms, not just the dates.
    pub exchange_ratio: Option<f64>,
}

#[tauri::command]
pub async fn list_link_proposals(state: State<'_, AppState>)
    -> Result<Vec<LinkProposal>, AppError> {
    Ok(sqlx::query_as::<_, LinkProposal>(
        "SELECT l.id, l.predecessor_id, l.successor_id,
                bp.label AS predecessor_label, bs.label AS successor_label,
                l.link_type, l.effective_date, l.evidence, l.exchange_ratio
           FROM instrument_link l
           LEFT JOIN book_entry bp ON bp.instrument_id = l.predecessor_id
           LEFT JOIN book_entry bs ON bs.instrument_id = l.successor_id
          WHERE l.confirmed_by IS NULL
          ORDER BY l.effective_date DESC")
        .fetch_all(&state.pool).await?)
}

#[tauri::command]
pub async fn confirm_link(state: State<'_, AppState>, link_id: i64)
    -> Result<(), AppError> {
    crate::instrument::store::confirm_link(&state.pool, link_id, "user").await
}

/// P9 Task 9: manual roll-link creation -- a human typing the junction IS the
/// confirmation gate (P0 7.2's human-agrees rule, satisfied at entry time
/// rather than via the review queue).
#[tauri::command]
pub async fn create_roll_link(state: State<'_, AppState>, predecessor_id: i64,
                              successor_id: i64, effective_date: String,
                              roll_offset: Option<f64>) -> Result<i64, AppError> {
    let d = chrono::NaiveDate::parse_from_str(&effective_date, "%Y-%m-%d")
        .map_err(|e| AppError::Validation(format!("bad effective_date: {e}")))?;
    crate::stitch::create_roll_link(&state.pool, predecessor_id, successor_id, d,
                                    roll_offset).await
}

// ---------------------------------------------------------------------------
// Fields
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_fields(state: State<'_, AppState>) -> Result<Vec<fields::FieldDef>, AppError> {
    fields::list_fields(&state.pool).await
}

#[tauri::command]
#[allow(clippy::too_many_arguments)]
pub async fn create_field(state: State<'_, AppState>, asset_class_id: i64,
                          mnemonic: String, label: String, value_kind: String,
                          bbg_ftype: Option<String>, bbg_datatype: Option<String>,
                          entitlement_note: Option<String>,
                          qc_nonpositive: Option<bool>, qc_outlier_pct: Option<f64>,
                          qc_stale_days: Option<i32>)
    -> Result<fields::FieldDef, AppError> {
    fields::create_field(&state.pool, asset_class_id, &mnemonic, &label, &value_kind,
                         bbg_ftype.as_deref(), bbg_datatype.as_deref(),
                         entitlement_note.as_deref().unwrap_or(""),
                         qc_nonpositive.unwrap_or(false), qc_outlier_pct, qc_stale_days).await
}

#[tauri::command]
pub async fn update_field_cadence(state: State<'_, AppState>, id: i64,
    cadence: Option<String>, fetch_via: String) -> Result<(), AppError>
{
    fields::update_field_cadence(&state.pool, id, cadence.as_deref(), &fetch_via).await
}

// ---------------------------------------------------------------------------
// Views
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_views(state: State<'_, AppState>) -> Result<Vec<views::View>, AppError> {
    views::list_views(&state.pool).await
}

#[tauri::command]
pub async fn create_view(state: State<'_, AppState>, name: String, description: String)
    -> Result<views::View, AppError> {
    views::create_view(&state.pool, &name, &description).await
}

#[tauri::command]
pub async fn set_view_instruments(state: State<'_, AppState>, view_id: i64,
                                  instrument_ids: Vec<i64>)
    -> Result<(), AppError> {
    views::set_view_instruments(&state.pool, view_id, &instrument_ids).await
}

#[tauri::command]
pub async fn set_view_fields(state: State<'_, AppState>, view_id: i64, field_ids: Vec<i64>)
    -> Result<(), AppError> {
    views::set_view_fields(&state.pool, view_id, &field_ids).await
}

#[tauri::command]
pub async fn get_view_instruments(state: State<'_, AppState>, view_id: i64)
    -> Result<Vec<book::BookEntry>, AppError> {
    views::view_instruments(&state.pool, view_id).await
}

#[tauri::command]
pub async fn get_view_fields(state: State<'_, AppState>, view_id: i64)
    -> Result<Vec<views::ViewField>, AppError> {
    views::view_fields(&state.pool, view_id).await
}

#[tauri::command]
pub async fn estimate_view(state: State<'_, AppState>, view_id: i64)
    -> Result<EstimateOut, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let members = views::view_instruments(&state.pool, view_id).await?;
    let fields_db = views::view_fields(&state.pool, view_id).await?;
    // Only members with a security string valid today are actually fetchable;
    // this mirrors orchestrator::load_view so the estimate matches what a run
    // would really send.
    let gen: Vec<_> = members.iter().filter_map(|m| {
        m.security.clone().map(|security| crate::fetch::FetchAsset {
            instrument_id: m.instrument_id, asset_class_id: m.asset_class_id,
            class_name: String::new(), label: m.label.clone(),
            bdp_security: security })
    }).collect();
    let specs: Vec<_> = fields_db.iter().map(|vf| crate::fetch::FetchField {
        field_id: vf.def.id, asset_class_id: vf.def.asset_class_id,
        mnemonic: vf.def.mnemonic.clone(), value_kind: vf.def.value_kind.clone(),
        cadence: vf.effective_cadence.clone(), fetch_via: vf.def.fetch_via.clone() })
        .collect();
    let estimated = budget::estimate_eod_hits(&gen, &specs);
    let today_total = budget::today_hits(&state.pool).await?;
    let level = budget::check_level(estimated, today_total, cfg.soft_limit);
    Ok(EstimateOut { estimated, today_total, level })
}

#[tauri::command]
pub async fn budget_today(state: State<'_, AppState>) -> Result<BudgetToday, AppError> {
    Ok(BudgetToday {
        hits: budget::today_hits(&state.pool).await?,
        soft_limit: state.cfg.read().await.soft_limit,
    })
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn run_eod_now(state: State<'_, AppState>, view_id: i64, confirmed: bool)
    -> Result<RunOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    // Amendment A1: "Run now" fetches the previous trading day's close, not today's
    // live values (BDP cannot return "as of yesterday").
    let obs_date = previous_weekday(chrono::Local::now().date_naive());
    orchestrator::run_eod(&state.pool, &cfg, view_id, "manual", obs_date, confirmed).await
}

#[tauri::command]
pub async fn run_backfill_now(state: State<'_, AppState>, view_id: i64,
                              start: String, end: String,
                              instrument_ids: Option<Vec<i64>>, confirmed: bool)
    -> Result<RunOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let s = start.parse().map_err(|_| AppError::Validation("bad start date".into()))?;
    let e = end.parse().map_err(|_| AppError::Validation("bad end date".into()))?;
    orchestrator::run_backfill(&state.pool, &cfg, view_id, s, e,
                               instrument_ids.as_deref(), confirmed).await
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RunRow {
    pub id: i64, pub view_id: i64, pub kind: String, pub trigger_kind: String,
    pub status: String, pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub estimated_hits: i64, pub error_summary: Option<String>,
}

#[tauri::command]
pub async fn list_runs(state: State<'_, AppState>, limit: i64)
    -> Result<Vec<RunRow>, AppError> {
    Ok(sqlx::query_as::<_, RunRow>(
        "SELECT id, view_id, kind, trigger_kind, status, started_at,
                finished_at, estimated_hits, error_summary
         FROM run ORDER BY id DESC LIMIT $1")
        .bind(limit).fetch_all(&state.pool).await?)
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct IssueRow {
    pub id: i64,
    /// NULL for an issue that arose outside any run (lifecycle findings,
    /// identifier-history refusals) -- see the schema comment on ingest_issue.
    pub run_id: Option<i64>,
    pub instrument_id: Option<i64>, pub field_id: Option<i64>,
    pub obs_date: Option<chrono::NaiveDate>, pub severity: String,
    pub code: String, pub detail: String,
    /// Book label of the instrument, when it has one -- an issue naming
    /// "instrument 2" is unreadable next to one naming "YODA LN Equity".
    pub label: Option<String>,
}

#[tauri::command]
pub async fn list_issues(state: State<'_, AppState>, run_id: i64)
    -> Result<Vec<IssueRow>, AppError> {
    Ok(sqlx::query_as::<_, IssueRow>(
        "SELECT i.id, i.run_id, i.instrument_id, i.field_id, i.obs_date,
                i.severity, i.code, i.detail, b.label
           FROM ingest_issue i
           LEFT JOIN book_entry b ON b.instrument_id = i.instrument_id
          WHERE i.run_id = $1 ORDER BY i.id")
        .bind(run_id).fetch_all(&state.pool).await?)
}

/// P6: issues that belong to no run -- lifecycle findings and
/// identifier-history refusals. They were durable in the database from the
/// start but had no surface in the UI; this is that surface.
#[tauri::command]
pub async fn list_standalone_issues(state: State<'_, AppState>)
    -> Result<Vec<IssueRow>, AppError> {
    Ok(sqlx::query_as::<_, IssueRow>(
        "SELECT i.id, i.run_id, i.instrument_id, i.field_id, i.obs_date,
                i.severity, i.code, i.detail, b.label
           FROM ingest_issue i
           LEFT JOIN book_entry b ON b.instrument_id = i.instrument_id
          WHERE i.run_id IS NULL ORDER BY i.id DESC LIMIT 200")
        .fetch_all(&state.pool).await?)
}

/// Gaps are per instrument, not per view: one member reporting on a date does
/// not mean the others did. The gap row's Backfill button passes the gap's
/// own instrument to `run_backfill_now`, so closing a one-name hole costs
/// that one name's hits, not the whole view's.
#[derive(Debug, Serialize)]
pub struct GapRow {
    pub instrument_id: i64,
    pub label: String,
    pub start: String,
    pub end: String,
    /// P11 11.5: `Some("2026-07")` when the hole is a whole missing period
    /// rather than a run of weekdays. The report renders the label instead of
    /// a day range, which for a monthly series is the only honest shape.
    pub period: Option<String>,
}

#[tauri::command]
pub async fn detect_view_gaps(state: State<'_, AppState>, view_id: i64)
    -> Result<Vec<GapRow>, AppError> {
    let today = chrono::Local::now().date_naive();
    Ok(scheduler::detect_gaps(&state.pool, view_id, 30, today).await?
        .into_iter()
        // P11 11.5: period-shaped gaps are withheld from this report until
        // Task 7 teaches the screen what one is. Every row here carries a live
        // Backfill button wired to `run_backfill_now(start, end)`, and a
        // period's bounds are not a day range: a 31-day month trips
        // BACKFILL_CAP_DAYS, and a shorter one buys a month of DAILY history
        // that can never close the gap (periodic fields are excluded from the
        // daily leg by design). The scheduler's own path already filters them
        // (`gap_backfill`); this is the manual half of the same rule.
        // Task 7 lifts this line together with the label rendering and a
        // button that dispatches the periodic leg instead.
        .filter(|g| g.period.is_none())
        .map(|g| GapRow { instrument_id: g.instrument_id, label: g.label,
                          start: g.start.to_string(), end: g.end.to_string(),
                          period: g.period })
        .collect())
}

// ---------------------------------------------------------------------------
// Schedules
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ScheduleRow {
    pub id: i64, pub view_id: i64, pub active: bool,
    pub window_start: chrono::NaiveTime, pub window_end: chrono::NaiveTime,
    pub drawn_for: Option<chrono::NaiveDate>, pub drawn_at: Option<chrono::NaiveTime>,
    pub last_result: Option<String>,
    pub verify_dow: Option<i16>, pub last_verified_on: Option<chrono::NaiveDate>,
}

#[tauri::command]
pub async fn list_schedules(state: State<'_, AppState>) -> Result<Vec<ScheduleRow>, AppError> {
    Ok(sqlx::query_as::<_, ScheduleRow>(
        "SELECT id, view_id, active, window_start, window_end,
                drawn_for, drawn_at, last_result, verify_dow, last_verified_on
         FROM schedule ORDER BY view_id")
        .fetch_all(&state.pool).await?)
}

#[tauri::command]
pub async fn upsert_schedule(state: State<'_, AppState>, view_id: i64,
                             window_start: String, window_end: String, active: bool,
                             verify_dow: Option<i16>)
    -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active, verify_dow)
         VALUES ($1, $2::time, $3::time, $4, $5)
         ON CONFLICT (view_id) DO UPDATE
           SET window_start = EXCLUDED.window_start,
               window_end = EXCLUDED.window_end,
               active = EXCLUDED.active,
               verify_dow = EXCLUDED.verify_dow,
               drawn_for = NULL, drawn_at = NULL")  // window changed: force a fresh draw
        .bind(view_id).bind(window_start).bind(window_end).bind(active).bind(verify_dow)
        .execute(&state.pool).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<AppConfig, AppError> {
    Ok(state.cfg.read().await.clone())
}

#[tauri::command]
pub async fn save_settings(state: State<'_, AppState>, cfg: AppConfig)
    -> Result<(), AppError> {
    // Config lives at the fixed default location (C:\bloomdata\config.json) so that
    // changing data_dir can't orphan it; data_dir only governs pending/archive workbooks.
    let default_data_dir = &AppConfig::default().data_dir;
    let path = PathBuf::from(default_data_dir).join("config.json");
    std::fs::create_dir_all(default_data_dir)?;
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)
        .map_err(|e| AppError::Validation(e.to_string()))?)?;
    *state.cfg.write().await = cfg;
    Ok(())
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------
//
// The UI calls describe_deletion to render its dialog, but every delete_*
// command re-checks its own invariants. The dialog is a courtesy; the command
// is the enforcement.

#[tauri::command]
pub async fn describe_deletion(state: State<'_, AppState>,
                               kind: deletion::EntityKind, id: i64)
    -> Result<deletion::DeletionImpact, AppError> {
    deletion::describe_deletion(&state.pool, kind, id).await
}

#[tauri::command]
pub async fn delete_asset(state: State<'_, AppState>, id: i64, mode: deletion::DeleteMode)
    -> Result<(), AppError> {
    deletion::delete_book_entry(&state.pool, id, mode).await
}

#[tauri::command]
pub async fn delete_field(state: State<'_, AppState>, id: i64, mode: deletion::DeleteMode)
    -> Result<(), AppError> {
    deletion::delete_field(&state.pool, id, mode).await
}

#[tauri::command]
pub async fn delete_view(state: State<'_, AppState>, id: i64, mode: deletion::DeleteMode)
    -> Result<(), AppError> {
    deletion::delete_view(&state.pool, id, mode).await
}

#[tauri::command]
pub async fn delete_asset_class(state: State<'_, AppState>, id: i64)
    -> Result<(), AppError> {
    deletion::delete_asset_class(&state.pool, id).await
}

#[tauri::command]
pub async fn delete_schedule(state: State<'_, AppState>, id: i64)
    -> Result<(), AppError> {
    deletion::delete_schedule(&state.pool, id).await
}

// ---------------------------------------------------------------------------
// Bulk assets
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn export_assets_xlsx(state: State<'_, AppState>, path: String)
    -> Result<(), AppError> {
    bulk::export_assets_xlsx(&state.pool, &PathBuf::from(path)).await
}

#[tauri::command]
pub async fn preview_assets_import(state: State<'_, AppState>, path: String)
    -> Result<bulk::diff::ImportPlan, AppError> {
    bulk::preview_import(&state.pool, &PathBuf::from(path)).await
}

#[tauri::command]
pub async fn apply_assets_import(state: State<'_, AppState>, path: String,
                                 file_hash: String,
                                 removal_modes: Vec<(i64, deletion::DeleteMode)>,
                                 confirmed_removal_count: Option<i64>)
    -> Result<bulk::ImportResult, AppError> {
    let cfg = pipeline_cfg(&state).await;
    bulk::apply_import(&state.pool, &cfg, &PathBuf::from(path), &file_hash,
                       &removal_modes, confirmed_removal_count).await
}

// ---------------------------------------------------------------------------
// Instrument search
// ---------------------------------------------------------------------------

/// Local search. Never calls Bloomberg -- this is the command behind the
/// type-ahead, and it runs on every keystroke.
#[tauri::command]
pub async fn search_local(state: State<'_, AppState>, query: String, limit: i64)
    -> Result<Vec<search::SearchHit>, AppError> {
    search::local(&state.pool, &query, limit.clamp(1, 50)).await
}

/// Explicit Bloomberg search. The UI must call this only from the
/// "Search Bloomberg" button -- never on input, focus or navigation.
#[tauri::command]
pub async fn search_bloomberg(state: State<'_, AppState>, query: String,
                              yellow_key: String)
    -> Result<search::BloombergSearch, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    search::bloomberg(&state.pool, &fetcher, &query, &yellow_key).await
}

// ---------------------------------------------------------------------------
// Instrument detail (aliases & attributes)
// ---------------------------------------------------------------------------
//
// Thin wrappers over `instrument::store`, exposed now so `api.ts` can stay a
// single unit; Task 16's InstrumentDetail panel is the first consumer.

#[tauri::command]
pub async fn instrument_aliases(state: State<'_, AppState>, instrument_id: i64)
    -> Result<Vec<crate::instrument::store::Alias>, AppError> {
    crate::instrument::store::aliases(&state.pool, instrument_id).await
}

/// Every attribute we have ever believed, not just today's, so the timeline
/// shows the change rather than only its result.
#[tauri::command]
pub async fn instrument_attrs(state: State<'_, AppState>, instrument_id: i64)
    -> Result<Vec<crate::instrument::store::Attr>, AppError> {
    crate::instrument::store::attrs_history(&state.pool, instrument_id).await
}

// ---------------------------------------------------------------------------
// Corporate actions (P3)
// ---------------------------------------------------------------------------

/// Explicit, costed user action (2 hits, charged at the wire seam under
/// purpose 'corp_actions'), same pattern as identifier history: never
/// automatic, never scheduled in P3.
#[tauri::command]
pub async fn refresh_corp_actions(state: State<'_, AppState>, instrument_id: i64)
    -> Result<crate::corp_actions::RefreshSummary, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    let as_of = chrono::Local::now().date_naive();
    crate::corp_actions::refresh(&state.pool, &fetcher, instrument_id, as_of).await
}

#[tauri::command]
pub async fn list_corp_actions(state: State<'_, AppState>, instrument_id: i64)
    -> Result<Vec<crate::corp_actions::ActionRow>, AppError> {
    crate::corp_actions::list_current(&state.pool, instrument_id).await
}

/// P6: the merger-lifecycle check, on demand. The same flow runs
/// automatically after every completed run; this button exists so a user
/// staring at a quiet instrument does not have to wait for the next run.
/// Costed like everything else -- 1 hit per stale instrument, more only for
/// the dead ones -- and bounded by the same 30-day cooldown.
#[tauri::command]
pub async fn run_lifecycle_check(state: State<'_, AppState>)
    -> Result<crate::lifecycle::LifecycleSummary, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    crate::lifecycle::run_check(&state.pool, &fetcher,
                                chrono::Local::now().date_naive()).await
}

// ---------------------------------------------------------------------------
// Data tab (read-only; never calls Bloomberg)
// ---------------------------------------------------------------------------

/// P5: the queried instrument's series extended backward through confirmed
/// merger/rename links. Derived on read; never stored, never a Bloomberg call.
#[tauri::command]
pub async fn list_stitched(state: State<'_, AppState>, instrument_id: i64,
                           field_id: i64, mode: String, limit: i64)
    -> Result<crate::stitch::StitchedSeries, AppError> {
    let m = crate::adjust::parse_mode(&mode)?;
    crate::stitch::stitched_series(&state.pool, instrument_id, field_id, m, limit).await
}

#[tauri::command]
pub async fn has_confirmed_predecessors(state: State<'_, AppState>,
                                        instrument_id: i64)
    -> Result<bool, AppError> {
    crate::stitch::has_confirmed_predecessors(&state.pool, instrument_id).await
}

#[tauri::command]
pub async fn export_stitched_csv(state: State<'_, AppState>, instrument_id: i64,
                                 field_id: i64, mode: String, path: String)
    -> Result<u64, AppError> {
    let m = crate::adjust::parse_mode(&mode)?;
    crate::stitch::export_stitched_csv(&state.pool, instrument_id, field_id, m,
                                       &PathBuf::from(path)).await
}

/// P4: derived on read from RAW observations + the factor chain. Never
/// stored, never a Bloomberg call.
#[tauri::command]
pub async fn list_adjusted(state: State<'_, AppState>, instrument_id: i64,
                           field_id: i64, mode: String, limit: i64)
    -> Result<crate::adjust::AdjSeries, AppError> {
    let m = crate::adjust::parse_mode(&mode)?;
    crate::adjust::adjusted_series(&state.pool, instrument_id, field_id, m, limit).await
}

#[tauri::command]
pub async fn export_adjusted_csv(state: State<'_, AppState>, instrument_id: i64,
                                 field_id: i64, mode: String, path: String)
    -> Result<u64, AppError> {
    let m = crate::adjust::parse_mode(&mode)?;
    crate::adjust::export_adjusted_csv(&state.pool, instrument_id, field_id, m,
                                       &PathBuf::from(path)).await
}

#[tauri::command]
pub async fn list_observations(state: State<'_, AppState>, instrument_id: i64,
                               field_id: i64, include_superseded: bool, limit: i64)
    -> Result<Vec<crate::dataview::ObsRow>, AppError> {
    crate::dataview::observations(&state.pool, instrument_id, field_id,
                                  include_superseded, limit).await
}

#[tauri::command]
pub async fn list_corp_actions_full(state: State<'_, AppState>, instrument_id: i64,
                                    include_superseded: bool)
    -> Result<Vec<crate::dataview::CorpActionFull>, AppError> {
    crate::dataview::corp_actions_full(&state.pool, instrument_id,
                                       include_superseded).await
}

#[tauri::command]
pub async fn export_observations_csv(state: State<'_, AppState>, instrument_id: i64,
                                     field_id: i64, path: String)
    -> Result<u64, AppError> {
    crate::dataview::export_observations_csv(&state.pool, instrument_id, field_id,
                                             &PathBuf::from(path)).await
}

#[tauri::command]
pub async fn export_corp_actions_csv(state: State<'_, AppState>, instrument_id: i64,
                                     path: String) -> Result<u64, AppError> {
    crate::dataview::export_corp_actions_csv(&state.pool, instrument_id,
                                             &PathBuf::from(path)).await
}

/// Whole-view refresh: batched Bloomberg calls (100 securities each), 2 hits
/// per instrument, charged at the wire seam. Still an explicit user action.
#[tauri::command]
pub async fn refresh_view_corp_actions(state: State<'_, AppState>, view_id: i64)
    -> Result<crate::corp_actions::ViewRefreshSummary, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    let as_of = chrono::Local::now().date_naive();
    // A click is an explicit retry: do NOT skip not-applicable instruments.
    crate::corp_actions::refresh_view(&state.pool, &fetcher, view_id, as_of, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P10 task 7: a config.json written before database_url/blp_host/blp_port
    /// existed must still parse -- the new fields land as None, never a hard
    /// error, via `#[serde(default)]`.
    #[test]
    fn old_config_json_still_parses() {
        let cfg: AppConfig = serde_json::from_str(
            r#"{"data_dir":"C:\\bloomdata","soft_limit":100000,"request_timeout_s":120,"python_path":"python"}"#)
            .unwrap();
        assert_eq!(cfg.database_url, None);
        assert_eq!(cfg.blp_port, None);
        assert_eq!(cfg.blp_host, None);
    }
}
