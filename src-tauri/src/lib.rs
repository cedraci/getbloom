pub mod blp_driver;
pub mod budget;
pub mod commands;
pub mod db;
pub mod deletion;
pub mod error;
pub mod fetch;
pub mod fields;
pub mod ingest;
pub mod orchestrator;
pub mod registry;
pub mod scheduler;
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
    let url = std::env::var("BLOOM_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/bloomdata".into());
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
            commands::list_assets, commands::create_asset, commands::set_asset_active,
            commands::list_fields, commands::create_field,
            commands::list_views, commands::create_view,
            commands::set_view_assets, commands::set_view_fields,
            commands::get_view_assets, commands::get_view_fields,
            commands::estimate_view, commands::run_eod_now, commands::run_backfill_now,
            commands::list_runs, commands::list_issues, commands::detect_view_gaps,
            commands::list_schedules, commands::upsert_schedule,
            commands::get_settings, commands::save_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
