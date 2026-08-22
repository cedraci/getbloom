pub mod adjust;
pub mod blp_driver;
pub mod book;
pub mod budget;
pub mod bulk;
pub mod commands;
pub mod corp_actions;
pub mod dataview;
pub mod db;
pub mod deletion;
pub mod error;
pub mod fetch;
pub mod fields;
pub mod ingest;
pub mod instrument;
pub mod lifecycle;
pub mod master_fetch;
pub mod orchestrator;
pub mod quality;
pub mod registry;
pub mod resolution;
pub mod scheduler;
pub mod stitch;
pub mod views;

use commands::{AppConfig, AppState};

// Config lives at the fixed default location (C:\bloomdata\config.json) so that
// changing data_dir can't orphan it; data_dir only governs pending/archive workbooks.
fn load_config() -> AppConfig {
    let default = AppConfig::default();
    let path = std::path::PathBuf::from(&default.data_dir).join("config.json");
    std::fs::read_to_string(path).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let cfg = load_config();
    // Precedence is deliberately UI-first: config.json -> BLOOM_DATABASE_URL env
    // -> hardcoded default. The user who edits the UI must see an effect, even
    // with the env var set. Takes effect on restart (config.json is read once,
    // here, before the pool connects).
    let url = cfg.database_url.clone()
        .or_else(|| std::env::var("BLOOM_DATABASE_URL").ok())
        .unwrap_or_else(|| "postgres://postgres:postgres@localhost/bloomdata".into());
    let pool = rt.block_on(db::connect(&url)).expect("database connection + migrations");

    let state = AppState { pool: pool.clone(), cfg: tokio::sync::RwLock::new(cfg.clone()) };

    // Tell Tauri to dispatch #[tauri::command] async fns onto this same runtime — the
    // one that owns the sqlx pool's connections and the heartbeat task below — rather
    // than spinning up its own internally-managed runtime. Must be called before any
    // tauri::async_runtime use and before the Builder is constructed/run. `rt` is a
    // local that lives for the rest of `run()`, so it outlives the Tauri event loop.
    tauri::async_runtime::set(rt.handle().clone());

    // scheduler heartbeat: every 60 s, fire any schedule whose drawn time has passed
    rt.spawn({
        let pool = pool.clone();
        async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                iv.tick().await;
                let cfg = load_config(); // reload persisted config each tick so settings edits apply
                let pc = orchestrator::PipelineConfig {
                    data_dir: std::path::PathBuf::from(&cfg.data_dir),
                    python_path: std::path::PathBuf::from(&cfg.python_path),
                    script_path: commands::script_path(),
                    request_timeout_s: cfg.request_timeout_s,
                    soft_limit: cfg.soft_limit,
                    blp_host: cfg.blp_host.clone(),
                    blp_port: cfg.blp_port,
                };
                if let Err(e) = scheduler::tick(&pool, &pc, chrono::Local::now()).await {
                    eprintln!("scheduler tick failed: {e}");
                }
            }
        }
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::list_asset_classes, commands::create_asset_class,
            commands::update_asset_class_capabilities,
            commands::list_book, commands::add_to_book, commands::set_book_active,
            commands::list_pending_reviews, commands::resolve_review,
            commands::resolve_review_local, commands::reject_review,
            commands::ingest_identifier_history,
            commands::list_link_proposals, commands::confirm_link,
            commands::list_fields, commands::create_field, commands::update_field_cadence,
            commands::list_views, commands::create_view,
            commands::set_view_instruments, commands::set_view_fields,
            commands::get_view_instruments, commands::get_view_fields,
            commands::estimate_view, commands::budget_today, commands::run_eod_now, commands::run_backfill_now,
            commands::list_runs, commands::list_issues, commands::detect_view_gaps,
            commands::list_schedules, commands::upsert_schedule,
            commands::describe_deletion,
            commands::delete_asset, commands::delete_field, commands::delete_view,
            commands::delete_asset_class, commands::delete_schedule,
            commands::get_settings, commands::save_settings,
            commands::export_assets_xlsx, commands::preview_assets_import,
            commands::apply_assets_import,
            commands::search_local, commands::search_bloomberg,
            commands::instrument_aliases, commands::instrument_attrs,
            commands::refresh_corp_actions, commands::list_corp_actions,
            commands::run_lifecycle_check, commands::list_standalone_issues,
            commands::refresh_view_corp_actions,
            commands::list_observations, commands::list_corp_actions_full,
            commands::export_observations_csv, commands::export_corp_actions_csv,
            commands::list_adjusted, commands::export_adjusted_csv,
            commands::list_stitched, commands::has_confirmed_predecessors,
            commands::export_stitched_csv, commands::create_roll_link,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
