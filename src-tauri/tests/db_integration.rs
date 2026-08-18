use getbloomdata_lib::error::{AppError, AppResult};
use getbloomdata_lib::fetch::{CellProblem, CellValue, FetchOutcome, FetchRequest, ObsCell};
use getbloomdata_lib::orchestrator;
use sqlx::Row;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

static SEQ: AtomicU32 = AtomicU32::new(0);

/// Unique fixture name, distinct both ACROSS tests in one run (they execute on
/// parallel threads against one database) and ACROSS repeated runs (these tests
/// commit real rows and never clean up).
///
/// Without this, `asset_crud_round_trip` and `eod_pipeline_dry_run_ends_partial`
/// both insert the security "AAPL US Equity" and whichever runs second dies on
/// UNIQUE (bdp_security) -- a failure on the very first run, not just re-runs.
fn uniq(stem: &str) -> String {
    static TAG: OnceLock<String> = OnceLock::new();
    let tag = TAG.get_or_init(|| {
        let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        format!("{:x}", nanos % 0x100000)
    });
    format!("{stem}{tag}{}", SEQ.fetch_add(1, Ordering::Relaxed))
}

fn test_url() -> Option<String> {
    std::env::var("BLOOM_TEST_DATABASE_URL").ok()
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn migration_creates_all_tables() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let rows = sqlx::query(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get("table_name")).collect();
    for t in ["asset_class","asset","field_def","view","view_asset","view_field",
              "run","observation","ingest_issue","hit_ledger","schedule"] {
        assert!(names.iter().any(|n| n == t), "missing table {t}");
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn asset_crud_round_trip() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let class = getbloomdata_lib::registry::create_asset_class(&pool, &uniq("EquityT3"), "test").await.unwrap();
    let ticker = format!("{} US", uniq("AAPL"));
    let asset = getbloomdata_lib::registry::create_asset(&pool, getbloomdata_lib::registry::NewAsset {
        asset_class_id: class.id,
        label: "Apple".into(),
        id_kind: "ticker".into(),
        ticker: Some(ticker.clone()),
        isin: None,
        yellow_key: "Equity".into(),
    }).await.unwrap();
    assert_eq!(asset.bdp_security, format!("{ticker} Equity"));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn view_fields_falls_back_to_class_fields() {
    use getbloomdata_lib::{db, fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, &uniq("EquityT4"), "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "MC".into(), id_kind: "isin".into(),
        ticker: None, isin: Some(uniq("FR000012")), yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, &uniq("luxt4"), "").await.unwrap();
    views::set_view_assets(&pool, v.id, &[a.id]).await.unwrap();
    let fs = views::view_fields(&pool, v.id).await.unwrap();  // no explicit fields
    assert!(fs.iter().any(|x| x.id == f.id));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn ingest_twice_converges_no_duplicates() {
    use getbloomdata_lib::{db, fields, ingest, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, &uniq("EquityT9"), "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST_T9", "px", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "T9".into(), id_kind: "ticker".into(),
        ticker: Some(format!("{} US", uniq("T9"))), isin: None, yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, &uniq("t9view"), "").await.unwrap();
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ingesting') RETURNING id")
        .bind(v.id).fetch_one(&pool).await.unwrap();

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
    let mk = |val: f64| FetchOutcome {
        cells: vec![ObsCell { asset_id: a.id, field_id: f.id,
                              obs_date: d, value: CellValue::Num(val) }],
        problems: vec![],
    };
    ingest::ingest_outcome(&pool, run_id, &mk(100.0)).await.unwrap();
    ingest::ingest_outcome(&pool, run_id, &mk(101.5)).await.unwrap();  // re-run: update, not dup

    let (count, val): (i64, f64) = sqlx::query_as(
        "SELECT count(*)::bigint, max(value_num)
         FROM observation WHERE asset_id = $1 AND field_id = $2")
        .bind(a.id).bind(f.id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
    assert_eq!(val, 101.5);
}

// A fetcher that returns canned data. This is the payoff of the reshaped
// `DataFetcher` trait (spec A2 §2.4): the orchestrator's whole path is now
// exercisable with no Bloomberg, no Excel, and no network. Under the Excel-era
// signature it could not be written at all -- that trait demanded a workbook
// path and a `WbMeta`.
struct MockFetcher {
    cells: Vec<ObsCell>,
    problems: Vec<CellProblem>,
    fail: bool,
}

impl orchestrator::DataFetcher for MockFetcher {
    async fn fetch(&self, _req: &FetchRequest, _audit: Option<&std::path::Path>)
        -> AppResult<FetchOutcome> {
        if self.fail {
            return Err(AppError::Blp { code: 3, detail: "mock session failure".into() });
        }
        Ok(FetchOutcome { cells: self.cells.clone(), problems: self.problems.clone() })
    }
}

/// Creates a class + numeric field + asset + view, returns (view_id, asset_id, field_id).
async fn seed_view(pool: &sqlx::PgPool) -> (i64, i64, i64) {
    use getbloomdata_lib::{fields, registry, views};
    let class = registry::create_asset_class(pool, &uniq("EquityE2E"), "t").await.unwrap();
    let f = fields::create_field(pool, class.id, "PX_LAST", "px", "numeric").await.unwrap();
    let a = registry::create_asset(pool, registry::NewAsset {
        asset_class_id: class.id, label: "E2E".into(), id_kind: "ticker".into(),
        ticker: Some(format!("{} US", uniq("E2E"))), isin: None, yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(pool, &uniq("e2eview"), "").await.unwrap();
    views::set_view_assets(pool, v.id, &[a.id]).await.unwrap();
    (v.id, a.id, f.id)
}

fn mock_cfg(dir: &std::path::Path) -> orchestrator::PipelineConfig {
    orchestrator::PipelineConfig {
        data_dir: dir.to_path_buf(),
        python_path: std::path::PathBuf::from("python"),
        script_path: std::path::PathBuf::from("scripts/blp_fetch.py"),
        request_timeout_s: 60,
        soft_limit: 100_000,
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn eod_pipeline_ingests_and_ends_ok() {
    use getbloomdata_lib::{db, orchestrator};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let (view_id, asset_id, field_id) = seed_view(&pool).await;

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
    let fetcher = MockFetcher {
        cells: vec![ObsCell { asset_id, field_id, obs_date: d,
                              value: CellValue::Num(305.59) }],
        problems: vec![],
        fail: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = mock_cfg(dir.path());
    let out = orchestrator::run_eod_with(&pool, &cfg, &fetcher, view_id, "manual", d, false)
        .await.unwrap();

    match out {
        orchestrator::RunOutcome::Completed { run_id, summary } => {
            assert_eq!(summary.upserted, 1);
            assert_eq!(summary.issues, 0);
            let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "ok");

            // The observation actually landed, with the right value and date.
            let v: f64 = sqlx::query_scalar(
                "SELECT value_num FROM observation
                 WHERE asset_id=$1 AND field_id=$2 AND obs_date=$3")
                .bind(asset_id).bind(field_id).bind(d)
                .fetch_one(&pool).await.unwrap();
            assert_eq!(v, 305.59);

            // Budget is recorded for every fetch attempt.
            let ledger: i64 = sqlx::query_scalar(
                "SELECT count(*)::bigint FROM hit_ledger WHERE run_id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(ledger, 1);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn eod_pipeline_with_problems_ends_partial() {
    use getbloomdata_lib::{db, orchestrator};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let (view_id, asset_id, field_id) = seed_view(&pool).await;

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
    // The holiday shape: no data, one problem, nothing to ingest.
    let fetcher = MockFetcher {
        cells: vec![],
        problems: vec![CellProblem {
            asset_id: Some(asset_id), field_id: Some(field_id), obs_date: Some(d),
            code: "no_data".into(), detail: "no trading day returned".into() }],
        fail: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let out = orchestrator::run_eod_with(&pool, &mock_cfg(dir.path()), &fetcher,
                                         view_id, "scheduled", d, false).await.unwrap();
    match out {
        orchestrator::RunOutcome::Completed { run_id, summary } => {
            assert_eq!(summary.upserted, 0);
            assert_eq!(summary.issues, 1);
            let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "partial");
            let code: String = sqlx::query_scalar(
                "SELECT code FROM ingest_issue WHERE run_id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(code, "no_data");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn eod_pipeline_fetch_failure_marks_run_failed() {
    use getbloomdata_lib::{db, orchestrator};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let (view_id, _, _) = seed_view(&pool).await;

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
    let fetcher = MockFetcher { cells: vec![], problems: vec![], fail: true };
    let dir = tempfile::tempdir().unwrap();
    let err = orchestrator::run_eod_with(&pool, &mock_cfg(dir.path()), &fetcher,
                                         view_id, "scheduled", d, false).await.unwrap_err();
    assert!(err.to_string().contains("mock session failure"), "got: {err}");

    // The run row must reach a terminal state, with the diagnosis stored --
    // never left dangling in `fetching`.
    let (status, summary): (String, Option<String>) = sqlx::query_as(
        "SELECT status, error_summary FROM run WHERE view_id=$1 ORDER BY id DESC LIMIT 1")
        .bind(view_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "failed");
    assert!(summary.unwrap().contains("mock session failure"));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_draw_persists_within_day() {
    use getbloomdata_lib::{db, scheduler, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let v = views::create_view(&pool, &uniq("t12view"), "").await.unwrap();
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO schedule (view_id) VALUES ($1) RETURNING id")
        .bind(v.id).fetch_one(&pool).await.unwrap();
    let today = chrono::Local::now().date_naive();
    let first = scheduler::ensure_draw(&pool, sid, today).await.unwrap();
    let second = scheduler::ensure_draw(&pool, sid, today).await.unwrap();
    assert_eq!(first, second);  // restart must not re-roll
    let win_s = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let win_e = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    assert!(first >= win_s && first < win_e);
}

// ---------------------------------------------------------------------------
// End-to-end smoke test (design §8, plan task 15)
//
// The one test that talks to the real Bloomberg Terminal. Idempotent by
// design: fixtures are looked up before being created, because `AAPL US
// Equity` can exist only once under UNIQUE (bdp_security).
// ---------------------------------------------------------------------------

const SMOKE_CLASS: &str = "SmokeEquity";
const SMOKE_SECURITY: &str = "AAPL US Equity";
const SMOKE_VIEW: &str = "smoke-view";

#[tokio::test]
#[ignore = "requires postgres AND a live Bloomberg Terminal"]
async fn smoke_real_bloomberg_end_to_end() {
    use getbloomdata_lib::{db, fields, orchestrator, registry, scheduler, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();

    // --- fixtures, get-or-create so the test can be run repeatedly ---
    let class_id: i64 = match sqlx::query_scalar("SELECT id FROM asset_class WHERE name=$1")
        .bind(SMOKE_CLASS).fetch_optional(&pool).await.unwrap() {
        Some(id) => id,
        None => registry::create_asset_class(&pool, SMOKE_CLASS, "smoke").await.unwrap().id,
    };
    for (mnemonic, kind) in [("PX_LAST", "numeric"), ("NAME", "text")] {
        let exists: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM field_def WHERE asset_class_id=$1 AND mnemonic=$2")
            .bind(class_id).bind(mnemonic).fetch_optional(&pool).await.unwrap();
        if exists.is_none() {
            fields::create_field(&pool, class_id, mnemonic, mnemonic, kind).await.unwrap();
        }
    }
    let asset_id: i64 = match sqlx::query_scalar("SELECT id FROM asset WHERE bdp_security=$1")
        .bind(SMOKE_SECURITY).fetch_optional(&pool).await.unwrap() {
        Some(id) => id,
        None => registry::create_asset(&pool, registry::NewAsset {
            asset_class_id: class_id, label: "Apple".into(), id_kind: "ticker".into(),
            ticker: Some("AAPL US".into()), isin: None, yellow_key: "Equity".into(),
        }).await.unwrap().id,
    };
    let view_id: i64 = match sqlx::query_scalar("SELECT id FROM view WHERE name=$1")
        .bind(SMOKE_VIEW).fetch_optional(&pool).await.unwrap() {
        Some(id) => id,
        None => views::create_view(&pool, SMOKE_VIEW, "smoke").await.unwrap().id,
    };
    views::set_view_assets(&pool, view_id, &[asset_id]).await.unwrap();

    // --- the real thing: previous trading day, real BLPAPI, real database ---
    let obs_date = scheduler::previous_weekday(chrono::Local::now().date_naive());
    let dir = tempfile::tempdir().unwrap();
    let cfg = orchestrator::PipelineConfig {
        data_dir: dir.path().to_path_buf(),
        python_path: std::path::PathBuf::from("python"),
        script_path: std::path::PathBuf::from("scripts/blp_fetch.py"),
        request_timeout_s: 120,
        soft_limit: 100_000,
    };
    let out = orchestrator::run_eod(&pool, &cfg, view_id, "manual", obs_date, true)
        .await.expect("live BLPAPI run failed");

    let orchestrator::RunOutcome::Completed { run_id, summary } = out else {
        panic!("expected Completed, got {out:?}");
    };
    eprintln!("smoke: run {run_id} obs_date={obs_date} \
               upserted={} issues={}", summary.upserted, summary.issues);

    // Both fields came back: PX_LAST via HistoricalDataRequest, NAME via
    // ReferenceDataRequest, both stamped with the same previous-day obs_date.
    let px: f64 = sqlx::query_scalar(
        "SELECT o.value_num FROM observation o JOIN field_def f ON f.id=o.field_id
         WHERE o.asset_id=$1 AND f.mnemonic='PX_LAST' AND o.obs_date=$2")
        .bind(asset_id).bind(obs_date).fetch_one(&pool).await
        .expect("no PX_LAST observation ingested");
    assert!(px > 0.0, "implausible price {px}");

    let name: String = sqlx::query_scalar(
        "SELECT o.value_text FROM observation o JOIN field_def f ON f.id=o.field_id
         WHERE o.asset_id=$1 AND f.mnemonic='NAME' AND o.obs_date=$2")
        .bind(asset_id).bind(obs_date).fetch_one(&pool).await
        .expect("no NAME observation ingested");
    assert!(name.to_uppercase().contains("APPLE"), "got name {name:?}");
    eprintln!("smoke: {name} PX_LAST={px} on {obs_date}");

    // The raw response was archived as the audit trail (spec A2 §4.4).
    let payload: Option<String> = sqlx::query_scalar(
        "SELECT payload_path FROM run WHERE id=$1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    let payload = payload.expect("run has no payload_path");
    assert!(std::path::Path::new(&payload).exists(), "missing audit payload {payload}");
}

/// `observation.run_id` is NOT NULL REFERENCES run(id), so any test that plants
/// an observation must plant a run first. `run.status` is CHECK-constrained to
/// ('generating','refreshing','reading','ingesting','ok','failed','partial') --
/// 'completed' is not a legal value.
async fn new_run(pool: &sqlx::PgPool, view_id: i64) -> i64 {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO run (view_id, kind, trigger_kind, status, estimated_hits)
         VALUES ($1, 'eod', 'manual', 'ok', 1) RETURNING id")
        .bind(view_id).fetch_one(pool).await.unwrap();
    id
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn describe_deletion_counts_what_hangs_off_an_asset() {
    use getbloomdata_lib::deletion::{describe_deletion, EntityKind};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("DescribeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Describe Me".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("DSC"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("DescribeView"), "")
        .await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();

    let run_id = new_run(&pool, view.id).await;
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $3),
                ($1, $2, DATE '2026-08-11', 2.0, $3)")
        .bind(asset.id).bind(field.id).bind(run_id)
        .execute(&pool).await.unwrap();

    let impact = describe_deletion(&pool, EntityKind::Asset, asset.id).await.unwrap();
    assert_eq!(impact.label, "Describe Me");
    assert_eq!(impact.observations, 2);
    assert_eq!(impact.first_obs, Some("2026-08-10".parse().unwrap()));
    assert_eq!(impact.last_obs, Some("2026-08-11".parse().unwrap()));
    assert_eq!(impact.views, 1);
    assert!(impact.can_retire && impact.can_purge);
    assert!(impact.blocked_reason.is_none());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn describe_deletion_blocks_a_non_empty_asset_class() {
    use getbloomdata_lib::deletion::{describe_deletion, EntityKind};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("BlockedCls"), "test").await.unwrap();
    getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Occupant".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("OCC"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();

    let impact = describe_deletion(&pool, EntityKind::AssetClass, class.id).await.unwrap();
    assert_eq!(impact.children, 1);
    assert!(!impact.can_retire, "an asset class has no retired state");
    assert!(!impact.can_purge, "a class with an asset in it cannot be deleted");
    assert!(impact.blocked_reason.is_some());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn delete_schedule_removes_the_row() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let view = getbloomdata_lib::views::create_view(&pool, &uniq("SchedView"), "").await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '18:00', TIME '19:00', true)")
        .bind(view.id).execute(&pool).await.unwrap();
    let (sid,): (i64,) = sqlx::query_as("SELECT id FROM schedule WHERE view_id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();

    getbloomdata_lib::deletion::delete_schedule(&pool, sid).await.unwrap();

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM schedule WHERE id = $1")
        .bind(sid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn delete_asset_class_refuses_while_occupied_then_succeeds_when_empty() {
    use getbloomdata_lib::error::AppError;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("EmptyMeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Tenant".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("TEN"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();

    let err = getbloomdata_lib::deletion::delete_asset_class(&pool, class.id).await.unwrap_err();
    assert!(matches!(err, AppError::DeleteBlocked { .. }),
            "expected DeleteBlocked, got {err:?}");

    sqlx::query("DELETE FROM asset WHERE id = $1").bind(asset.id)
        .execute(&pool).await.unwrap();
    getbloomdata_lib::deletion::delete_asset_class(&pool, class.id).await.unwrap();

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset_class WHERE id = $1")
        .bind(class.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn retiring_an_asset_hides_it_from_views_but_keeps_its_observations() {
    use getbloomdata_lib::deletion::{delete_asset, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("RetireCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Retiree".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("RET"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("RetireView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();
    let run_id = new_run(&pool, view.id).await;
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $3)")
        .bind(asset.id).bind(field.id).bind(run_id).execute(&pool).await.unwrap();

    assert_eq!(getbloomdata_lib::views::view_assets(&pool, view.id).await.unwrap().len(), 1);

    delete_asset(&pool, asset.id, DeleteMode::Retire).await.unwrap();

    assert!(getbloomdata_lib::views::view_assets(&pool, view.id).await.unwrap().is_empty(),
            "a retired asset must drop out of view resolution");
    let (obs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM observation WHERE asset_id = $1")
        .bind(asset.id).fetch_one(&pool).await.unwrap();
    assert_eq!(obs, 1, "retire never destroys collected data");
    let (row,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset WHERE id = $1")
        .bind(asset.id).fetch_one(&pool).await.unwrap();
    assert_eq!(row, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_an_asset_clears_its_data_but_leaves_run_and_hit_ledger() {
    use getbloomdata_lib::deletion::{delete_asset, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("PurgeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Doomed".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("DOOM"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("PurgeView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();

    let run_id = new_run(&pool, view.id).await;
    sqlx::query("INSERT INTO hit_ledger (run_id, estimated_hits) VALUES ($1, 7)")
        .bind(run_id).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $3)")
        .bind(asset.id).bind(field.id).bind(run_id).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, asset_id, field_id, obs_date, severity, code, detail)
         VALUES ($1, $2, $3, DATE '2026-08-10', 'warn', 'no_data', 'test')")
        .bind(run_id).bind(asset.id).bind(field.id).execute(&pool).await.unwrap();

    delete_asset(&pool, asset.id, DeleteMode::Purge).await.unwrap();

    for (table, col) in [("observation", "asset_id"), ("ingest_issue", "asset_id"),
                         ("view_asset", "asset_id"), ("asset", "id")] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE {col} = $1"))
            .bind(asset.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should have no rows for the purged asset");
    }
    let (runs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(runs, 1, "a purge must never rewrite the record of work that happened");
    let (hits,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM hit_ledger WHERE run_id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(hits, 1, "the budget ledger records money spent and is never rewritten");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_a_field_clears_its_observations_and_memberships() {
    use getbloomdata_lib::deletion::{delete_field, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("FldPurgeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Holder".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("HLD"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_VOLUME", "Volume", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("FldPurgeView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_fields(&pool, view.id, &[field.id]).await.unwrap();
    let run_id = new_run(&pool, view.id).await;
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 5.0, $3)")
        .bind(asset.id).bind(field.id).bind(run_id).execute(&pool).await.unwrap();

    delete_field(&pool, field.id, DeleteMode::Purge).await.unwrap();

    for (table, col) in [("observation", "field_id"), ("ingest_issue", "field_id"),
                         ("view_field", "field_id"), ("field_def", "id")] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE {col} = $1"))
            .bind(field.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should have no rows for the purged field");
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_view_with_runs_refuses_to_purge_but_still_retires() {
    use getbloomdata_lib::deletion::{delete_view, DeleteMode};
    use getbloomdata_lib::error::AppError;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let view = getbloomdata_lib::views::create_view(&pool, &uniq("HistoricView"), "").await.unwrap();
    sqlx::query(
        "INSERT INTO run (view_id, kind, trigger_kind, status, estimated_hits)
         VALUES ($1, 'eod', 'manual', 'ok', 1)")
        .bind(view.id).execute(&pool).await.unwrap();

    let err = delete_view(&pool, view.id, DeleteMode::Purge).await.unwrap_err();
    assert!(matches!(err, AppError::DeleteBlocked { .. }), "got {err:?}");

    delete_view(&pool, view.id, DeleteMode::Retire).await.unwrap();
    let (active,): (bool,) = sqlx::query_as("SELECT active FROM view WHERE id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();
    assert!(!active);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_a_never_run_view_takes_its_schedule_and_memberships_with_it() {
    use getbloomdata_lib::deletion::{delete_view, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("VPurgeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Member".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("MEM"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("FreshView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '18:00', TIME '19:00', true)")
        .bind(view.id).execute(&pool).await.unwrap();

    delete_view(&pool, view.id, DeleteMode::Purge).await.unwrap();

    for table in ["schedule", "view_asset", "view_field"] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE view_id = $1"))
            .bind(view.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should be empty for the purged view");
    }
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM view WHERE id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
    // The asset itself is untouched: a view is a selection, not an owner.
    let (a,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset WHERE id = $1")
        .bind(asset.id).fetch_one(&pool).await.unwrap();
    assert_eq!(a, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn scheduler_skips_schedules_whose_view_is_retired() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let view = getbloomdata_lib::views::create_view(&pool, &uniq("RetiredSched"), "").await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '00:01', TIME '00:02', true)")
        .bind(view.id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE view SET active = false WHERE id = $1")
        .bind(view.id).execute(&pool).await.unwrap();

    let due = getbloomdata_lib::scheduler::due_schedules(&pool).await.unwrap();
    assert!(!due.iter().any(|(_, vid, _)| *vid == view.id),
            "a retired view must not appear in the due list");
}
