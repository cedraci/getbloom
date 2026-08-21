//! P10 production-ops behaviours: honest ledger, gap backfill, current_eod.
mod common;
use common::uniq;
use chrono::NaiveDate;
fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// The wire shape `master_fetch::parse_identity` reads: one securityData
/// entry, no FIGI, no errors. Mirrors `db_integration.rs`'s fixture helper --
/// fixtures go through the real `book::add` path with Bloomberg mocked, never
/// by hand-seeding `instrument_alias` rows directly.
fn identity_raw_for(security: &str) -> serde_json::Value {
    serde_json::json!([{"securityData": [{
        "security": security,
        "fieldExceptions": [], "sequenceNumber": 0,
        "fieldData": {}
    }]}])
}

/// Build a book entry through `book::add`, with Bloomberg mocked to answer
/// with exactly one identity block for `{ticker} Equity`.
async fn add_book_entry(
    pool: &sqlx::PgPool,
    class_id: i64,
    label: &str,
    ticker: &str,
) -> getbloomdata_lib::book::BookEntry {
    use getbloomdata_lib::book::{self, AddOutcome, AddToBook};
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    use getbloomdata_lib::resolution::score::Hints;

    let security = format!("{ticker} Equity");
    let fetcher = MockMasterFetcher {
        identity_raw: identity_raw_for(&security),
        ..Default::default()
    };
    let req = AddToBook {
        raw: ticker.to_string(),
        yellow_key: "Equity".into(),
        asset_class_id: class_id,
        label: label.into(),
        hints: Hints::default(),
    };
    match book::add(pool, &fetcher, &req, "test").await.unwrap() {
        AddOutcome::Added(entry) => entry,
        other => panic!("expected Added for {ticker}, got {other:?}"),
    }
}

/// The hit ledger must record what was actually dispatched to Bloomberg, not
/// the pre-flight gate estimate. `asset_class.corp_actions_capable` defaults
/// to true (migration 0011), so this fixture's lone book entry is priced by
/// `orchestrator::corp_actions_estimate` too: the gate estimate is 1 EOD hit
/// + 2 corp-action hits = 3. Before this task, `record_hits` charged that
/// whole gate estimate into the ledger even though the corp-action leg bills
/// itself separately at the wire seam (master_fetch.rs) -- a double count.
/// The ledger must show only the 1 hit this run's single security x single
/// field x single day actually dispatched.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_ledger_records_dispatched_hits_not_the_gate_estimate() {
    use getbloomdata_lib::error::AppResult;
    use getbloomdata_lib::fetch::{FetchOutcome, FetchRequest};
    use getbloomdata_lib::orchestrator::{self, DataFetcher, PipelineConfig};
    use getbloomdata_lib::{fields, registry, views};
    use std::path::Path;

    struct EmptyFetcher;
    impl DataFetcher for EmptyFetcher {
        async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            Ok(FetchOutcome::default())
        }
    }

    let pool = common::pool().await;
    let class = registry::create_asset_class(&pool, &uniq("OpsCls"), "t").await.unwrap();
    let field = fields::create_field(
        &pool, class.id, "PX_LAST", "Last", "numeric",
        None, None, "", false, None, None).await.unwrap();
    let entry = add_book_entry(&pool, class.id, "OpsA", &uniq("OPSA")).await;
    let view = views::create_view(&pool, &uniq("opsview"), "").await.unwrap();
    views::set_view_instruments(&pool, view.id, &[entry.instrument_id]).await.unwrap();
    views::set_view_fields(&pool, view.id, &[field.id]).await.unwrap();

    let cfg = PipelineConfig {
        data_dir: std::env::temp_dir(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit: 1_000_000,
    };
    let day = d("2026-08-17");
    let out = orchestrator::run_eod_with(
        &pool, &cfg, &EmptyFetcher, view.id, "manual", day, true)
        .await.unwrap();
    let run_id = match out {
        orchestrator::RunOutcome::Completed { run_id, .. } => run_id,
        other => panic!("expected Completed, got {other:?}"),
    };

    // The fixture must actually carry a corp-action leg for this test to
    // prove anything -- confirm the gate estimate is inflated by it.
    let estimated: i64 = sqlx::query_scalar("SELECT estimated_hits FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert!(estimated > 1,
            "fixture's corp_actions_capable class should inflate the gate estimate, got {estimated}");

    let hits: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits), 0)::bigint FROM hit_ledger WHERE run_id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(hits, 1, "ledger must record exactly what was dispatched, not the gate estimate");
}

// ---------------------------------------------------------------------------
// Gap auto-backfill after downtime.
// ---------------------------------------------------------------------------

use getbloomdata_lib::error::AppResult;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, FetchRequest, ObsCell};
use getbloomdata_lib::orchestrator::{
    self, DataFetcher, GapBackfillOutcome, PipelineConfig,
};
use getbloomdata_lib::{fields, registry, views};
use std::path::Path;

/// Serves one numeric cell per (asset, class-matching field, weekday) of
/// whatever range it is handed -- the canned-fetcher idiom of
/// `tests/pipeline.rs`, widened to a multi-day range so a backfill actually
/// closes the holes it was launched for.
struct DayFetcher;
impl DataFetcher for DayFetcher {
    async fn fetch(&self, req: &FetchRequest, _audit: Option<&Path>)
        -> AppResult<FetchOutcome> {
        let mut cells = Vec::new();
        let mut day = req.start;
        while day <= req.end {
            if !getbloomdata_lib::scheduler::is_weekend(day) {
                for a in &req.assets {
                    for f in req.fields.iter()
                                .filter(|f| f.asset_class_id == a.asset_class_id) {
                        cells.push(ObsCell {
                            instrument_id: a.instrument_id, field_id: f.field_id,
                            obs_date: day, value: CellValue::Num(1.0) });
                    }
                }
            }
            day += chrono::Duration::days(1);
        }
        Ok(FetchOutcome { cells, problems: vec![] })
    }
}

fn gap_cfg(dir: &Path, soft_limit: i64) -> PipelineConfig {
    PipelineConfig {
        data_dir: dir.to_path_buf(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit,
    }
}

/// A one-instrument view with a HOLEY observation history: the machine went
/// down after Thu 2026-08-13 and came back on Wed 2026-08-19. Mon 08-10
/// through Thu 08-13 are present; Fri 08-14, Mon 08-17 and Tue 08-18 are not.
///
/// Tue 08-18 is deliberately missing as well: it is the day today's own EOD
/// run targets, so the gap backfill must leave it alone. Returns
/// `(view_id, instrument_id)`.
async fn gap_fixture(pool: &sqlx::PgPool, stem: &str) -> (i64, i64) {
    let class = registry::create_asset_class(pool, &uniq("GapCls"), "t").await.unwrap();
    let field = fields::create_field(
        pool, class.id, "PX_LAST", "Last", "numeric",
        None, None, "", false, None, None).await.unwrap();
    let entry = add_book_entry(pool, class.id, stem, &uniq(stem)).await;
    let view = views::create_view(pool, &uniq("gapview"), "").await.unwrap();
    views::set_view_instruments(pool, view.id, &[entry.instrument_id]).await.unwrap();
    views::set_view_fields(pool, view.id, &[field.id]).await.unwrap();

    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(view.id).fetch_one(pool).await.unwrap();
    for day in ["2026-08-10", "2026-08-11", "2026-08-12", "2026-08-13"] {
        sqlx::query(
            "INSERT INTO observation
               (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
             VALUES ($1,$2,$3,'raw',1,100.0,$4)")
            .bind(entry.instrument_id).bind(field.id).bind(d(day)).bind(rid)
            .execute(pool).await.unwrap();
    }
    (view.id, entry.instrument_id)
}

async fn scheduled_backfill_runs(pool: &sqlx::PgPool, view_id: i64) -> i64 {
    sqlx::query_scalar(
        "SELECT count(*)::bigint FROM run
          WHERE view_id = $1 AND kind = 'backfill' AND trigger_kind = 'scheduled'")
        .bind(view_id).fetch_one(pool).await.unwrap()
}

/// After downtime the scheduler fills the weekdays it missed before the day's
/// own run -- and the horizon stops strictly before the day today's EOD will
/// fetch, or yesterday would look like a gap every single morning.
#[tokio::test]
#[ignore = "requires postgres"]
async fn gap_backfill_fills_missed_weekdays_and_records_scheduled_backfill_runs() {
    let pool = common::pool().await;
    let (vid, iid) = gap_fixture(&pool, "GapA").await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = gap_cfg(dir.path(), 1_000_000);

    let out = orchestrator::run_gap_backfill_with(
        &pool, &cfg, &DayFetcher, vid, d("2026-08-19")).await.unwrap();
    match out {
        GapBackfillOutcome::Ran { days, .. } =>
            assert_eq!(days, 2, "Fri 08-14 and Mon 08-17, and nothing newer"),
        other => panic!("expected Ran, got {other:?}"),
    }

    let n = scheduled_backfill_runs(&pool, vid).await;
    assert!(n >= 1, "the recovery must be visible as a scheduled backfill run");

    let filled: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM observation
          WHERE instrument_id = $1 AND system_to = 'infinity'
            AND obs_date IN (DATE '2026-08-14', DATE '2026-08-17')")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(filled, 2, "both holes must actually be closed");

    // The horizon stops one weekday short of what the day's EOD will fetch.
    // Tue 08-18 is missing too, but it is TODAY's run's job -- backfilling it
    // here would pay for the same day twice, every single morning.
    let too_new: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM observation
          WHERE instrument_id = $1 AND obs_date >= DATE '2026-08-18'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(too_new, 0, "the gap backfill must not reach into the EOD run's day");

    // A gap run must never stand in for the day's EOD: `already_ran_today`
    // counts eod/verify only, so the normal run still fires afterwards.
    let today = chrono::Local::now().date_naive();
    assert!(!getbloomdata_lib::scheduler::already_ran_today(&pool, vid, today)
                .await.unwrap(),
            "a backfill must not suppress the day's EOD run");
}

/// A scheduler cannot click a confirm box, so anything above `BudgetLevel::Ok`
/// runs NOTHING and reports. There is no hard cap and no self-confirmation.
#[tokio::test]
#[ignore = "requires postgres"]
async fn gap_backfill_stops_at_the_soft_limit_instead_of_confirming_itself() {
    let pool = common::pool().await;
    let (vid, _iid) = gap_fixture(&pool, "GapB").await;
    let dir = tempfile::tempdir().unwrap();
    let cfg_zero = gap_cfg(dir.path(), 0); // any estimate at all lands past Ok

    let out = orchestrator::run_gap_backfill_with(
        &pool, &cfg_zero, &DayFetcher, vid, d("2026-08-19")).await.unwrap();
    assert!(matches!(out, GapBackfillOutcome::NeedsConfirmation { .. }),
            "expected NeedsConfirmation, got {out:?}");
    assert_eq!(scheduled_backfill_runs(&pool, vid).await, 0,
               "nothing may run past Ok without a human");
}

/// One attempt per day, whatever its status: a gap that cannot be filled must
/// not be retried on every heartbeat for the rest of the day.
#[tokio::test]
#[ignore = "requires postgres"]
async fn gap_backfill_attempts_at_most_once_per_day() {
    let pool = common::pool().await;
    let (vid, _iid) = gap_fixture(&pool, "GapC").await;
    let dir = tempfile::tempdir().unwrap();
    let cfg = gap_cfg(dir.path(), 1_000_000);

    let first = orchestrator::run_gap_backfill_with(
        &pool, &cfg, &DayFetcher, vid, d("2026-08-19")).await.unwrap();
    assert!(matches!(first, GapBackfillOutcome::Ran { .. }),
            "expected Ran, got {first:?}");
    let after_first = scheduled_backfill_runs(&pool, vid).await;

    let second = orchestrator::run_gap_backfill_with(
        &pool, &cfg, &DayFetcher, vid, d("2026-08-19")).await.unwrap();
    assert!(matches!(second, GapBackfillOutcome::AlreadyAttemptedToday),
            "expected AlreadyAttemptedToday, got {second:?}");
    assert_eq!(scheduled_backfill_runs(&pool, vid).await, after_first,
               "the second call of the day must not launch anything");
}
