use crate::budget::{self, BudgetLevel};
use crate::error::{AppError, AppResult};
use crate::excel_gen::{self, GenAsset, GenField, WbMeta};
use crate::excel_read::{self, FieldSpec};
use crate::ingest::{self, IngestSummary};
use crate::refresh_driver;
use crate::views;
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;
use std::path::{Path, PathBuf};

pub const BACKFILL_CAP_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub data_dir: PathBuf,
    pub script_path: PathBuf,
    pub refresh_timeout_s: u32,
    pub soft_limit: i64,
    pub dry_run_refresh: bool,
}

#[derive(Debug, Serialize)]
pub enum RunOutcome {
    Completed { run_id: i64, summary: IngestSummary },
    NeedsConfirmation { estimated: i64, today_total: i64 },
}

pub fn pending_path(data_dir: &Path, view_name: &str, date: NaiveDate) -> PathBuf {
    data_dir.join("pending").join(format!("{view_name}_{date}.xlsx"))
}

pub fn archive_path(data_dir: &Path, run_id: i64, view_name: &str, date: NaiveDate) -> PathBuf {
    data_dir.join("archive")
        .join(date.format("%Y").to_string())
        .join(date.format("%m").to_string())
        .join(format!("run_{run_id}_{view_name}_{date}.xlsx"))
}

struct Loaded {
    view_name: String,
    assets: Vec<GenAsset>,
    gen_fields: Vec<GenField>,
    field_specs: Vec<FieldSpec>,
}

async fn load_view(pool: &PgPool, view_id: i64) -> AppResult<Loaded> {
    let view = sqlx::query_as::<_, views::View>("SELECT * FROM view WHERE id = $1")
        .bind(view_id).fetch_one(pool).await?;
    let assets_db = views::view_assets(pool, view_id).await?;
    let fields_db = views::view_fields(pool, view_id).await?;
    let classes = crate::registry::list_asset_classes(pool).await?;
    let class_name = |id: i64| classes.iter().find(|c| c.id == id)
        .map(|c| c.name.clone()).unwrap_or_else(|| format!("Class{id}"));
    let assets = assets_db.iter().map(|a| GenAsset {
        asset_id: a.id, asset_class_id: a.asset_class_id,
        class_name: class_name(a.asset_class_id),
        label: a.label.clone(), bdp_security: a.bdp_security.clone(),
    }).collect();
    let gen_fields = fields_db.iter().map(|f| GenField {
        field_id: f.id, asset_class_id: f.asset_class_id, mnemonic: f.mnemonic.clone(),
    }).collect();
    let field_specs = fields_db.iter().map(|f| FieldSpec {
        field_id: f.id, asset_class_id: f.asset_class_id,
        mnemonic: f.mnemonic.clone(), value_kind: f.value_kind.clone(),
    }).collect();
    Ok(Loaded { view_name: view.name, assets, gen_fields, field_specs })
}

async fn set_status(pool: &PgPool, run_id: i64, status: &str) -> AppResult<()> {
    sqlx::query("UPDATE run SET status = $2 WHERE id = $1")
        .bind(run_id).bind(status).execute(pool).await?;
    Ok(())
}

async fn fail_run(pool: &PgPool, run_id: i64, err: &AppError) -> AppResult<()> {
    sqlx::query("UPDATE run SET status='failed', finished_at=now(), error_summary=$2 WHERE id=$1")
        .bind(run_id).bind(err.to_string()).execute(pool).await?;
    Ok(())
}

async fn refresh_with_retry(cfg: &PipelineConfig, wb: &Path) -> AppResult<()> {
    match refresh_driver::run_refresh(&cfg.script_path, wb,
                                      cfg.refresh_timeout_s, cfg.dry_run_refresh).await {
        Err(AppError::Refresh { code: 2, .. }) => {
            refresh_driver::run_refresh(&cfg.script_path, wb,
                                        cfg.refresh_timeout_s * 2, cfg.dry_run_refresh)
                .await.map(|_| ())
        }
        other => other.map(|_| ()),
    }
}

async fn finish(pool: &PgPool, cfg: &PipelineConfig, run_id: i64, view_name: &str,
                date: NaiveDate, wb: &Path, summary: IngestSummary) -> AppResult<RunOutcome> {
    let dest = archive_path(&cfg.data_dir, run_id, view_name, date);
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(wb, &dest)?;
    let status = if summary.issues > 0 { "partial" } else { "ok" };
    sqlx::query("UPDATE run SET status=$2, finished_at=now(), workbook_path=$3 WHERE id=$1")
        .bind(run_id).bind(status).bind(dest.to_string_lossy().as_ref())
        .execute(pool).await?;
    Ok(RunOutcome::Completed { run_id, summary })
}

pub async fn run_eod(pool: &PgPool, cfg: &PipelineConfig, view_id: i64,
                     trigger: &str, obs_date: NaiveDate, confirmed: bool)
    -> AppResult<RunOutcome> {
    let loaded = load_view(pool, view_id).await?;
    let estimated = budget::estimate_eod_hits(&loaded.assets, &loaded.field_specs);
    let today_total = budget::today_hits(pool).await?;
    if budget::check_level(estimated, today_total, cfg.soft_limit) == BudgetLevel::HardConfirm
        && !confirmed {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }

    let wb = pending_path(&cfg.data_dir, &loaded.view_name, obs_date);
    std::fs::create_dir_all(wb.parent().unwrap())?;
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, workbook_path, estimated_hits)
         VALUES ($1,'eod',$2,'generating',$3,$4) RETURNING id")
        .bind(view_id).bind(trigger)
        .bind(wb.to_string_lossy().as_ref()).bind(estimated)
        .fetch_one(pool).await?;

    let meta = WbMeta { run_id, view_id, kind: "eod".into(),
                        generated_at: chrono::Local::now().to_rfc3339() };

    set_status(pool, run_id, "refreshing").await?;
    let fetcher = ExcelComFetcher { cfg };
    let result = fetcher.fetch_eod(&wb, &meta, &loaded.assets, &loaded.gen_fields,
                                   &loaded.field_specs, obs_date).await;
    // Hits are recorded for every fetch attempt, even on failure — over-counting is
    // safer than under-counting for a budget guard.
    budget::record_hits(pool, run_id, estimated).await?;
    let outcome = match result {
        Ok(o) => o,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };

    set_status(pool, run_id, "reading").await?;
    set_status(pool, run_id, "ingesting").await?;
    let summary = match ingest::ingest_outcome(pool, run_id, &outcome).await {
        Ok(s) => s,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };
    finish(pool, cfg, run_id, &loaded.view_name, obs_date, &wb, summary).await
}

pub async fn run_backfill(pool: &PgPool, cfg: &PipelineConfig, view_id: i64,
                          start: NaiveDate, end: NaiveDate, confirmed: bool)
    -> AppResult<RunOutcome> {
    if start > end {
        return Err(AppError::Validation("start after end".into()));
    }
    if (end - start).num_days() + 1 > BACKFILL_CAP_DAYS {
        return Err(AppError::Validation(
            format!("backfill range exceeds {BACKFILL_CAP_DAYS}-day cap")));
    }
    let loaded = load_view(pool, view_id).await?;
    let estimated = budget::estimate_backfill_hits(&loaded.assets, &loaded.field_specs, start, end);
    let today_total = budget::today_hits(pool).await?;
    // Spec §5.3: every backfill shows its cost and requires explicit confirmation.
    if !confirmed {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }

    let wb = pending_path(&cfg.data_dir, &loaded.view_name, end);
    std::fs::create_dir_all(wb.parent().unwrap())?;
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, workbook_path, estimated_hits)
         VALUES ($1,'backfill','manual','generating',$2,$3) RETURNING id")
        .bind(view_id).bind(wb.to_string_lossy().as_ref()).bind(estimated)
        .fetch_one(pool).await?;

    let meta = WbMeta { run_id, view_id, kind: "backfill".into(),
                        generated_at: chrono::Local::now().to_rfc3339() };

    set_status(pool, run_id, "refreshing").await?;
    let fetcher = ExcelComFetcher { cfg };
    let result = fetcher.fetch_history(&wb, &meta, &loaded.assets, &loaded.gen_fields,
                                       &loaded.field_specs, start, end).await;
    // Hits are recorded for every fetch attempt, even on failure — over-counting is
    // safer than under-counting for a budget guard.
    budget::record_hits(pool, run_id, estimated).await?;
    let outcome = match result {
        Ok(o) => o,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };

    set_status(pool, run_id, "reading").await?;
    set_status(pool, run_id, "ingesting").await?;
    let summary = match ingest::ingest_outcome(pool, run_id, &outcome).await {
        Ok(s) => s,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };
    finish(pool, cfg, run_id, &loaded.view_name, end, &wb, summary).await
}

/// Seam for swapping the Excel/COM fetch path for direct BLPAPI later (spec §2.4).
/// The trait covers stages 1–3 (generate → refresh → read); ingest is fetcher-agnostic.
pub trait DataFetcher {
    fn fetch_eod(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                 gen_fields: &[GenField], field_specs: &[FieldSpec],
                 obs_date: NaiveDate)
        -> impl std::future::Future<Output = AppResult<excel_read::ReadOutcome>> + Send;
    fn fetch_history(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                     gen_fields: &[GenField], field_specs: &[FieldSpec],
                     start: NaiveDate, end: NaiveDate)
        -> impl std::future::Future<Output = AppResult<excel_read::ReadOutcome>> + Send;
}

pub struct ExcelComFetcher<'a> { pub cfg: &'a PipelineConfig }

impl DataFetcher for ExcelComFetcher<'_> {
    async fn fetch_eod(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                       gen_fields: &[GenField], field_specs: &[FieldSpec],
                       obs_date: NaiveDate) -> AppResult<excel_read::ReadOutcome> {
        excel_gen::generate_eod_workbook(wb, meta, assets, gen_fields)?;
        refresh_with_retry(self.cfg, wb).await?;
        excel_read::read_eod_workbook(wb, meta.run_id, assets, field_specs, obs_date)
    }
    async fn fetch_history(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                           gen_fields: &[GenField], field_specs: &[FieldSpec],
                           start: NaiveDate, end: NaiveDate)
        -> AppResult<excel_read::ReadOutcome> {
        excel_gen::generate_backfill_workbook(wb, meta, assets, gen_fields, start, end)?;
        refresh_with_retry(self.cfg, wb).await?;
        excel_read::read_backfill_workbook(wb, meta.run_id, assets, field_specs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::path::Path;

    #[test]
    fn pending_and_archive_paths_follow_spec_naming() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let p = pending_path(Path::new("C:\\bloomdata"), "core-eq", d);
        assert!(p.ends_with(Path::new("pending").join("core-eq_2026-08-13.xlsx")));
        let a = archive_path(Path::new("C:\\bloomdata"), 42, "core-eq", d);
        assert!(a.ends_with(Path::new("archive").join("2026").join("08")
            .join("run_42_core-eq_2026-08-13.xlsx")));
    }
}
