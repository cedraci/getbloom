//! The run pipeline (spec A2 §4): plan → fetch → ingest.
//!
//! Three stages instead of four. There is no generate stage (no workbook to
//! build) and reading is no longer separate from fetching, so `run_eod` and
//! `run_backfill` now differ only in their date range, their budget estimate,
//! and their confirmation rule — everything else runs through `execute`.

use crate::blp_driver;
use crate::budget::{self, BudgetLevel};
use crate::error::{AppError, AppResult};
use crate::fetch::{self, FetchAsset, FetchField, FetchOutcome, FetchRequest, SidecarPayload};
use crate::ingest::{self, IngestSummary};
use crate::views;
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;
use std::path::{Path, PathBuf};

pub const BACKFILL_CAP_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub data_dir: PathBuf,
    pub python_path: PathBuf,
    pub script_path: PathBuf,
    pub request_timeout_s: u32,
    pub soft_limit: i64,
}

#[derive(Debug, Serialize)]
pub enum RunOutcome {
    Completed {
        run_id: i64,
        summary: IngestSummary,
        /// Filled by the live wrappers: corporate actions ride with every
        /// run (user decision 2026-08-21). None when the refresh errored
        /// (reported, never fatal) or on paths that skip it.
        corp_actions: Option<crate::corp_actions::ViewRefreshSummary>,
    },
    NeedsConfirmation { estimated: i64, today_total: i64 },
}

/// Where a run's raw sidecar response is archived (spec A2 §4.4). Same
/// convention the workbooks used, so the audit trail keeps its shape.
pub fn payload_path(data_dir: &Path, run_id: i64, view_name: &str, date: NaiveDate) -> PathBuf {
    data_dir
        .join("archive")
        .join(date.format("%Y").to_string())
        .join(date.format("%m").to_string())
        .join(format!("run_{run_id}_{view_name}_{date}.json"))
}

// ---------------------------------------------------------------- the seam

/// Spec A2 §2.4. One method: an EOD run is a history request whose range is a
/// single day, which is exactly what Amendment A1 established.
///
/// `audit_path` is where the implementation should persist the raw upstream
/// response, if it has one; it is advisory and a fetcher may ignore it.
pub trait DataFetcher {
    fn fetch(
        &self,
        req: &FetchRequest,
        audit_path: Option<&Path>,
    ) -> impl std::future::Future<Output = AppResult<FetchOutcome>> + Send;
}

pub struct BlpapiFetcher<'a> {
    pub cfg: &'a PipelineConfig,
}

impl DataFetcher for BlpapiFetcher<'_> {
    async fn fetch(
        &self,
        req: &FetchRequest,
        audit_path: Option<&Path>,
    ) -> AppResult<FetchOutcome> {
        let payload = SidecarPayload {
            run_id: req.run_id,
            timeout_s: self.cfg.request_timeout_s,
            requests: fetch::plan_requests(req)?,
        };
        let resp = blp_driver::run_fetch(
            &self.cfg.python_path,
            &self.cfg.script_path,
            &payload,
            audit_path,
        )
        .await?;
        Ok(fetch::map_response(req, &resp))
    }
}

// ---------------------------------------------------------------- view load

struct Loaded {
    view_name: String,
    assets: Vec<FetchAsset>,
    fields: Vec<FetchField>,
}

async fn load_view(pool: &PgPool, view_id: i64, only: Option<&[i64]>) -> AppResult<Loaded> {
    let view = sqlx::query_as::<_, views::View>("SELECT * FROM view WHERE id = $1")
        .bind(view_id)
        .fetch_one(pool)
        .await?;
    let mut members = views::view_instruments(pool, view_id).await?;
    if let Some(ids) = only {
        // A filtered backfill (a per-instrument gap) fetches only its target;
        // an id not in the view is simply absent, and plan_requests' empty-
        // assets validation reports the net result.
        members.retain(|m| ids.contains(&m.instrument_id));
    }
    let fields_db = views::view_fields(pool, view_id).await?;
    let classes = crate::registry::list_asset_classes(pool).await?;
    let class_name = |id: i64| {
        classes
            .iter()
            .find(|c| c.id == id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("Class{id}"))
    };
    let mut assets = Vec::with_capacity(members.len());
    let today = chrono::Local::now().date_naive();
    for m in &members {
        // The security string is derived from the alias valid today, never read
        // off the book entry -- one instrument wears several over its life.
        let Some(security) = m.security.clone() else {
            // No security valid today: delisted, or never resolved. Skipping is
            // right, and saying so is what keeps it from looking like a holiday --
            // eprintln! alone reaches nobody in a Tauri binary, so it is also
            // recorded durably. No run row exists yet at this point in the flow
            // (load_view runs before execute() creates one), so run_id is NULL --
            // exactly what ingest_issue.run_id being nullable is for (see the
            // migration's comment on that column).
            eprintln!("view {view_id}: instrument {} has no security string today, skipping",
                      m.instrument_id);
            sqlx::query(
                "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
                 VALUES (NULL, $1, 'warn', 'no_security_today', $2)")
                .bind(m.instrument_id)
                .bind(format!(
                    "view {view_id}: instrument {} has no security string valid as of {today}, \
                     skipped",
                    m.instrument_id))
                .execute(pool).await?;
            continue;
        };
        assets.push(FetchAsset {
            instrument_id: m.instrument_id,
            asset_class_id: m.asset_class_id,
            class_name: class_name(m.asset_class_id),
            label: m.label.clone(),
            bdp_security: security,
        });
    }
    let mut fields = Vec::with_capacity(fields_db.len());
    for f in &fields_db {
        if f.bbg_ftype.as_deref() == Some("BulkFormat") {
            // plan_requests would coerce a table into one meaningless string
            // (the sidecar docstring's exact warning). Skipped and said out
            // loud; the data has its own path: the corporate-actions refresh.
            sqlx::query(
                "INSERT INTO ingest_issue (run_id, field_id, severity, code, detail)
                 VALUES (NULL, $1, 'warn', 'bulk_field_skipped', $2)")
                .bind(f.id)
                .bind(format!("bulk field {} skipped by the run pipeline; use \
                               the corporate-actions refresh instead", f.mnemonic))
                .execute(pool).await?;
            continue;
        }
        fields.push(FetchField {
            field_id: f.id,
            asset_class_id: f.asset_class_id,
            mnemonic: f.mnemonic.clone(),
            value_kind: f.value_kind.clone(),
        });
    }
    Ok(Loaded { view_name: view.name, assets, fields })
}

async fn set_status(pool: &PgPool, run_id: i64, status: &str) -> AppResult<()> {
    sqlx::query("UPDATE run SET status = $2 WHERE id = $1")
        .bind(run_id)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

async fn fail_run(pool: &PgPool, run_id: i64, err: &AppError) -> AppResult<()> {
    sqlx::query("UPDATE run SET status='failed', finished_at=now(), error_summary=$2 WHERE id=$1")
        .bind(run_id)
        .bind(err.to_string())
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------- the pipeline

#[allow(clippy::too_many_arguments)]
async fn execute<F: DataFetcher>(
    pool: &PgPool,
    cfg: &PipelineConfig,
    fetcher: &F,
    loaded: &Loaded,
    view_id: i64,
    kind: &str,
    trigger: &str,
    start: NaiveDate,
    end: NaiveDate,
    estimated: i64,
) -> AppResult<RunOutcome> {
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, estimated_hits)
         VALUES ($1,$2,$3,'fetching',$4) RETURNING id",
    )
    .bind(view_id)
    .bind(kind)
    .bind(trigger)
    .bind(estimated)
    .fetch_one(pool)
    .await?;

    let audit = payload_path(&cfg.data_dir, run_id, &loaded.view_name, end);
    if let Some(parent) = audit.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let req = FetchRequest {
        run_id,
        assets: loaded.assets.clone(),
        fields: loaded.fields.clone(),
        start,
        end,
    };
    let result = fetcher.fetch(&req, Some(&audit)).await;

    // Hits are recorded for every fetch attempt, even on failure — over-counting
    // is the safe direction for a budget guard. Losing this advisory ledger row
    // must not abort or mask the pipeline result.
    if let Err(e) = budget::record_hits(pool, run_id, estimated).await {
        eprintln!("warning: failed to record budget hit for run {run_id}: {e}");
    }

    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            let _ = fail_run(pool, run_id, &e).await;
            return Err(e);
        }
    };

    // Advisory, like the hit ledger: losing a holiday mark must not fail a
    // run that already fetched its data.
    if let Err(e) = ingest::record_non_trading_days(pool, &req, &outcome).await {
        eprintln!("warning: non-trading-day recording failed for run {run_id}: {e}");
    }

    set_status(pool, run_id, "ingesting").await?;
    let summary = match ingest::ingest_outcome(pool, run_id, &outcome).await {
        Ok(s) => s,
        Err(e) => {
            let _ = fail_run(pool, run_id, &e).await;
            return Err(e);
        }
    };

    let status = if summary.issues > 0 { "partial" } else { "ok" };
    let stored = audit.exists().then(|| audit.to_string_lossy().into_owned());
    sqlx::query("UPDATE run SET status=$2, finished_at=now(), payload_path=$3 WHERE id=$1")
        .bind(run_id)
        .bind(status)
        .bind(stored)
        .execute(pool)
        .await?;

    Ok(RunOutcome::Completed { run_id, summary, corp_actions: None })
}

// ---------------------------------------------------------------- entry points

pub async fn run_eod(
    pool: &PgPool,
    cfg: &PipelineConfig,
    view_id: i64,
    trigger: &str,
    obs_date: NaiveDate,
    confirmed: bool,
) -> AppResult<RunOutcome> {
    let mut result = run_eod_with(pool, cfg, &BlpapiFetcher { cfg }, view_id, trigger,
                                  obs_date, confirmed).await;
    auto_reresolve_after(pool, cfg, &result).await;
    corp_actions_after(pool, cfg, view_id, &mut result).await;
    lifecycle_after(pool, cfg, &result).await;
    result
}

/// Live wrappers only: after a completed run, try to re-point instruments
/// whose security came back invalid_security (a rename discovered the hard
/// way). Advisory -- a recovery failure must not fail a run that already
/// ingested its data. The `_with` variants never call this, so every
/// mock-fetcher test is untouched.
async fn auto_reresolve_after(pool: &PgPool, cfg: &PipelineConfig,
                              result: &AppResult<RunOutcome>) {
    if let Ok(RunOutcome::Completed { run_id, .. }) = result {
        let mf = crate::master_fetch::BlpapiMasterFetcher { cfg, pool };
        if let Err(e) = crate::resolution::engine::auto_reresolve_invalid(
            pool, &mf, *run_id, chrono::Local::now().date_naive()).await {
            eprintln!("auto re-resolve after run {run_id} failed: {e}");
        }
    }
}

/// Corporate actions ride with every completed run and backfill (user
/// decision 2026-08-21): factors and dividends must always sit beside the
/// prices they explain. skip_na = true -- instruments Bloomberg declared
/// not-applicable are not re-charged; the manual button is the retry. A
/// failure here is reported on stderr and the run stays Completed.
async fn corp_actions_after(pool: &PgPool, cfg: &PipelineConfig, view_id: i64,
                            result: &mut AppResult<RunOutcome>) {
    if let Ok(RunOutcome::Completed { run_id, corp_actions, .. }) = result {
        let mf = crate::master_fetch::BlpapiMasterFetcher { cfg, pool };
        match crate::corp_actions::refresh_view(
            pool, &mf, view_id, chrono::Local::now().date_naive(), true).await {
            Ok(sum) => *corp_actions = Some(sum),
            Err(e) => eprintln!("corp-actions refresh after run {run_id} failed: {e}"),
        }
    }
}

/// P6: after a completed run, ask ONE cheap question about book instruments
/// that have gone quiet (design: 2026-08-20-p6-merger-lifecycle-design.md).
/// On a healthy book `stale_candidates` is empty and this costs nothing.
/// Advisory like its two siblings: a lifecycle failure is reported on
/// stderr and durable issues, never by failing a run that already ingested.
async fn lifecycle_after(pool: &PgPool, cfg: &PipelineConfig,
                         result: &AppResult<RunOutcome>) {
    if let Ok(RunOutcome::Completed { run_id, .. }) = result {
        let mf = crate::master_fetch::BlpapiMasterFetcher { cfg, pool };
        match crate::lifecycle::run_check(
            pool, &mf, chrono::Local::now().date_naive()).await {
            Ok(s) if s.checked > 0 => eprintln!(
                "lifecycle after run {run_id}: {} checked, {} dead, \
                 {} links proposed, {} auto-confirmed, {} issues",
                s.checked, s.dead, s.links_proposed, s.links_confirmed, s.issues),
            Ok(_) => {}
            Err(e) => eprintln!("lifecycle check after run {run_id} failed: {e}"),
        }
    }
}

/// What the run's corp-action leg will cost: 2 hits per member that will
/// actually be requested (not flagged not-applicable). Advisory, for the
/// pre-run gate; the wire seam still charges exactly what is sent. Members
/// without a security today are still priced -- over-count-is-safe, the
/// standing estimate policy.
async fn corp_actions_estimate(pool: &PgPool, view_id: i64) -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM view_instrument vi
          WHERE vi.view_id = $1
            AND NOT EXISTS (SELECT 1 FROM corp_actions_na na
                             WHERE na.instrument_id = vi.instrument_id)")
        .bind(view_id).fetch_one(pool).await?;
    Ok(crate::master_fetch::corp_actions_hit_cost(n as usize))
}

pub async fn run_eod_with<F: DataFetcher>(
    pool: &PgPool,
    cfg: &PipelineConfig,
    fetcher: &F,
    view_id: i64,
    trigger: &str,
    obs_date: NaiveDate,
    confirmed: bool,
) -> AppResult<RunOutcome> {
    let loaded = load_view(pool, view_id, None).await?;
    // Prices + the corp-action leg that follows a completed run.
    let estimated = budget::estimate_eod_hits(&loaded.assets, &loaded.fields)
        + corp_actions_estimate(pool, view_id).await?;
    let today_total = budget::today_hits(pool).await?;
    if budget::check_level(estimated, today_total, cfg.soft_limit) == BudgetLevel::HardConfirm
        && !confirmed
    {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }
    execute(pool, cfg, fetcher, &loaded, view_id, "eod", trigger,
            obs_date, obs_date, estimated).await
}

#[allow(clippy::too_many_arguments)]
pub async fn run_backfill(
    pool: &PgPool,
    cfg: &PipelineConfig,
    view_id: i64,
    start: NaiveDate,
    end: NaiveDate,
    instrument_ids: Option<&[i64]>,
    confirmed: bool,
) -> AppResult<RunOutcome> {
    let mut result = run_backfill_with(pool, cfg, &BlpapiFetcher { cfg }, view_id, start, end,
                                       instrument_ids, confirmed).await;
    auto_reresolve_after(pool, cfg, &result).await;
    corp_actions_after(pool, cfg, view_id, &mut result).await;
    lifecycle_after(pool, cfg, &result).await;
    result
}

#[allow(clippy::too_many_arguments)]
pub async fn run_backfill_with<F: DataFetcher>(
    pool: &PgPool,
    cfg: &PipelineConfig,
    fetcher: &F,
    view_id: i64,
    start: NaiveDate,
    end: NaiveDate,
    instrument_ids: Option<&[i64]>,
    confirmed: bool,
) -> AppResult<RunOutcome> {
    if start > end {
        return Err(AppError::Validation("start after end".into()));
    }
    if (end - start).num_days() + 1 > BACKFILL_CAP_DAYS {
        return Err(AppError::Validation(format!(
            "backfill range exceeds {BACKFILL_CAP_DAYS}-day cap"
        )));
    }
    let loaded = load_view(pool, view_id, instrument_ids).await?;
    // Prices per weekday + ONE corp-action leg at the end (Bloomberg returns
    // the full history on every call; a backfill needs no per-day refresh).
    let estimated = budget::estimate_backfill_hits(&loaded.assets, &loaded.fields, start, end)
        + corp_actions_estimate(pool, view_id).await?;
    let today_total = budget::today_hits(pool).await?;
    // Spec §5.3: every backfill shows its cost and requires explicit confirmation.
    if !confirmed {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }
    execute(pool, cfg, fetcher, &loaded, view_id, "backfill", "manual",
            start, end, estimated).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{CellProblem, CellValue, ObsCell};
    use chrono::NaiveDate;
    use std::path::Path;

    #[test]
    fn payload_path_follows_the_archive_convention() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let p = payload_path(Path::new("C:\\bloomdata"), 42, "core-eq", d);
        assert!(p.ends_with(
            Path::new("archive").join("2026").join("08").join("run_42_core-eq_2026-08-13.json")
        ));
    }

    /// Returns canned data without touching Bloomberg — the whole point of the
    /// reshaped trait. Impossible with the Excel-era signature, which demanded
    /// a workbook path and a `WbMeta`.
    pub struct MockFetcher {
        pub cells: Vec<ObsCell>,
        pub problems: Vec<CellProblem>,
        pub fail: Option<&'static str>,
    }

    impl DataFetcher for MockFetcher {
        async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            if let Some(msg) = self.fail {
                return Err(AppError::Blp { code: 3, detail: msg.into() });
            }
            Ok(FetchOutcome {
                cells: self.cells.clone(),
                problems: self.problems.clone(),
            })
        }
    }

    #[tokio::test]
    async fn mock_fetcher_satisfies_the_trait() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
        let m = MockFetcher {
            cells: vec![ObsCell { instrument_id: 1, field_id: 2, obs_date: d,
                                  value: CellValue::Num(1.5) }],
            problems: vec![],
            fail: None,
        };
        let req = FetchRequest { run_id: 1, assets: vec![], fields: vec![], start: d, end: d };
        let out = m.fetch(&req, None).await.unwrap();
        assert_eq!(out.cells.len(), 1);

        let bad = MockFetcher { cells: vec![], problems: vec![], fail: Some("no session") };
        assert!(bad.fetch(&req, None).await.is_err());
    }
}
