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
