//! Run the P6 merger-lifecycle check against the real database and a live
//! Terminal, outside the app:
//!
//!     cargo run --example lifecycle_check
//!
//! Same flow the post-run hook and the run_lifecycle_check command execute;
//! same cooldowns; hits charged to hit_ledger at the wire seam as always.
//! Useful when a fund looks dead and the next scheduled run is hours away.

use getbloomdata_lib::commands::AppConfig;
use getbloomdata_lib::master_fetch::BlpapiMasterFetcher;
use getbloomdata_lib::orchestrator::PipelineConfig;

#[tokio::main]
async fn main() {
    let default = AppConfig::default();
    let path = std::path::PathBuf::from(&default.data_dir).join("config.json");
    let app_cfg: AppConfig = std::fs::read_to_string(&path).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default);
    let cfg = PipelineConfig {
        data_dir: std::path::PathBuf::from(&app_cfg.data_dir),
        python_path: std::path::PathBuf::from(&app_cfg.python_path),
        script_path: std::path::PathBuf::from("scripts/blp_fetch.py"),
        request_timeout_s: app_cfg.request_timeout_s,
        soft_limit: app_cfg.soft_limit,
        blp_host: app_cfg.blp_host,
        blp_port: app_cfg.blp_port,
    };
    let url = std::env::var("BLOOM_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/bloomdata".into());
    let pool = getbloomdata_lib::db::connect(&url).await.expect("db");

    let today = chrono::Local::now().date_naive();
    let candidates = getbloomdata_lib::lifecycle::stale_candidates(&pool, today)
        .await.expect("candidates");
    println!("{} stale candidate(s) as of {today}:", candidates.len());
    for (id, sec) in &candidates {
        println!("  instrument {id}: {sec}");
    }

    let fetcher = BlpapiMasterFetcher { cfg: &cfg, pool: &pool };
    let s = getbloomdata_lib::lifecycle::run_check(&pool, &fetcher, today)
        .await.expect("lifecycle check");
    println!("checked={} dead={} links_proposed={} auto_confirmed={} issues={}",
             s.checked, s.dead, s.links_proposed, s.links_confirmed, s.issues);
}
