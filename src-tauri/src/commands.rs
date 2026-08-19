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
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { data_dir: "C:\\bloomdata".into(),
               soft_limit: budget::DEFAULT_SOFT_LIMIT,
               request_timeout_s: 120,   // BLPAPI answers in seconds, not the minutes Excel needed
               python_path: "python".into() }
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
    }
}

#[derive(Debug, Serialize)]
pub struct EstimateOut {
    pub estimated: i64,
    pub today_total: i64,
    pub level: budget::BudgetLevel,
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
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg };
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
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg };
    engine::resolve_review(&state.pool, &fetcher, review_id, &chosen_security, "user").await
}

#[tauri::command]
pub async fn reject_review(state: State<'_, AppState>, review_id: i64, note: String)
    -> Result<(), AppError> {
    engine::reject_review(&state.pool, review_id, &note).await
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
}

#[tauri::command]
pub async fn list_link_proposals(state: State<'_, AppState>)
    -> Result<Vec<LinkProposal>, AppError> {
    Ok(sqlx::query_as::<_, LinkProposal>(
        "SELECT l.id, l.predecessor_id, l.successor_id,
                bp.label AS predecessor_label, bs.label AS successor_label,
                l.link_type, l.effective_date, l.evidence
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
                          entitlement_note: Option<String>)
    -> Result<fields::FieldDef, AppError> {
    fields::create_field(&state.pool, asset_class_id, &mnemonic, &label, &value_kind,
                         bbg_ftype.as_deref(), bbg_datatype.as_deref(),
                         entitlement_note.as_deref().unwrap_or("")).await
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
    -> Result<Vec<fields::FieldDef>, AppError> {
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
    let specs: Vec<_> = fields_db.iter().map(|f| crate::fetch::FetchField {
        field_id: f.id, asset_class_id: f.asset_class_id,
        mnemonic: f.mnemonic.clone(), value_kind: f.value_kind.clone() }).collect();
    let estimated = budget::estimate_eod_hits(&gen, &specs);
    let today_total = budget::today_hits(&state.pool).await?;
    let level = budget::check_level(estimated, today_total, cfg.soft_limit);
    Ok(EstimateOut { estimated, today_total, level })
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
                              start: String, end: String, confirmed: bool)
    -> Result<RunOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let s = start.parse().map_err(|_| AppError::Validation("bad start date".into()))?;
    let e = end.parse().map_err(|_| AppError::Validation("bad end date".into()))?;
    orchestrator::run_backfill(&state.pool, &cfg, view_id, s, e, confirmed).await
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
    pub id: i64, pub run_id: i64, pub instrument_id: Option<i64>, pub field_id: Option<i64>,
    pub obs_date: Option<chrono::NaiveDate>, pub severity: String,
    pub code: String, pub detail: String,
}

#[tauri::command]
pub async fn list_issues(state: State<'_, AppState>, run_id: i64)
    -> Result<Vec<IssueRow>, AppError> {
    Ok(sqlx::query_as::<_, IssueRow>(
        "SELECT id, run_id, instrument_id, field_id, obs_date, severity, code, detail
         FROM ingest_issue WHERE run_id = $1 ORDER BY id")
        .bind(run_id).fetch_all(&state.pool).await?)
}

#[tauri::command]
pub async fn detect_view_gaps(state: State<'_, AppState>, view_id: i64)
    -> Result<Vec<(String, String)>, AppError> {
    let today = chrono::Local::now().date_naive();
    Ok(scheduler::detect_gaps(&state.pool, view_id, 30, today).await?
        .into_iter().map(|(s, e)| (s.to_string(), e.to_string())).collect())
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
}

#[tauri::command]
pub async fn list_schedules(state: State<'_, AppState>) -> Result<Vec<ScheduleRow>, AppError> {
    Ok(sqlx::query_as::<_, ScheduleRow>(
        "SELECT id, view_id, active, window_start, window_end,
                drawn_for, drawn_at, last_result
         FROM schedule ORDER BY view_id")
        .fetch_all(&state.pool).await?)
}

#[tauri::command]
pub async fn upsert_schedule(state: State<'_, AppState>, view_id: i64,
                             window_start: String, window_end: String, active: bool)
    -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, $2::time, $3::time, $4)
         ON CONFLICT (view_id) DO UPDATE
           SET window_start = EXCLUDED.window_start,
               window_end = EXCLUDED.window_end,
               active = EXCLUDED.active,
               drawn_for = NULL, drawn_at = NULL")  // window changed: force a fresh draw
        .bind(view_id).bind(window_start).bind(window_end).bind(active)
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
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg };
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
