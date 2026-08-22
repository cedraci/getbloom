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

// ---------------------------------------------------------------------------
// Shared fixture helpers.
//
// Task 13B: fixtures are built through the real `book::add` path with a
// `MockMasterFetcher`, never by hand-seeding `instrument_alias` rows directly.
// A hand-seeded fixture already hid a Critical defect on this branch once (a
// guard that could never fire in production because nothing but the test
// ever wrote the row it keyed on), so every fixture here goes through the
// same resolution the app itself uses.
// ---------------------------------------------------------------------------

/// The wire shape `master_fetch::parse_identity` reads: one securityData
/// entry, no FIGI (fixtures never need one -- `instrument.id_bb_global` is
/// UNIQUE and the bdp_security alias already gives every fixture a unique
/// identity via `uniq()`), no errors.
fn identity_raw_for(security: &str) -> serde_json::Value {
    serde_json::json!([{"securityData": [{
        "security": security,
        "fieldExceptions": [], "sequenceNumber": 0,
        "fieldData": {}
    }]}])
}

/// Build a book entry through `book::add`, with Bloomberg mocked to answer
/// with exactly one identity block for `{ticker} Equity`. Returns the full
/// `BookEntry` so callers can read `instrument_id`, `security`, `label`, etc.
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

/// The adjustment_basis row migration 0001 seeds for "all four flags false" --
/// the RAW basis every numeric observation fixture below needs, since
/// `observation_numeric_needs_basis` requires a basis_id whenever value_num
/// is set.
async fn raw_basis_id(pool: &sqlx::PgPool) -> i16 {
    sqlx::query_scalar(
        "SELECT id FROM adjustment_basis
          WHERE adj_normal = false AND adj_abnormal = false
            AND adj_split = false AND adj_follow_dpdf = false",
    )
    .fetch_one(pool)
    .await
    .unwrap()
}

/// `observation.run_id` is NOT NULL REFERENCES run(id), so any test that plants
/// an observation must plant a run first. `run.status` is CHECK-constrained to
/// ('pending','fetching','ingesting','ok','failed','partial').
async fn new_run(pool: &sqlx::PgPool, view_id: i64) -> i64 {
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO run (view_id, kind, trigger_kind, status, estimated_hits)
         VALUES ($1, 'eod', 'manual', 'ok', 1) RETURNING id",
    )
    .bind(view_id)
    .fetch_one(pool)
    .await
    .unwrap();
    id
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
    for t in [
        "instrument", "resolution_decision", "instrument_attr", "instrument_alias",
        "instrument_link", "resolution_review", "book_entry", "instrument_candidate",
        "asset_class", "field_def", "view", "view_instrument", "view_field", "run",
        "adjustment_basis", "observation", "ingest_issue", "hit_ledger", "schedule",
    ] {
        assert!(names.iter().any(|n| n == t), "missing table {t}");
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn book_crud_round_trip() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let class = getbloomdata_lib::registry::create_asset_class(&pool, &uniq("EquityT3"), "test")
        .await
        .unwrap();
    let ticker = format!("{} US", uniq("AAPL"));
    let entry = add_book_entry(&pool, class.id, "Apple", &ticker).await;
    assert_eq!(entry.security, Some(format!("{ticker} Equity")));
    assert_eq!(entry.label, "Apple");
    assert!(entry.active);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn view_fields_falls_back_to_class_fields() {
    use getbloomdata_lib::{fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, &uniq("EquityT4"), "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric", None, None, "", false, None, None)
        .await
        .unwrap();
    let entry = add_book_entry(&pool, class.id, "MC", &format!("{} FP", uniq("MC"))).await;
    let v = views::create_view(&pool, &uniq("luxt4"), "").await.unwrap();
    views::set_view_instruments(&pool, v.id, &[entry.instrument_id]).await.unwrap();
    let fs = views::view_fields(&pool, v.id).await.unwrap(); // no explicit fields
    assert!(fs.iter().any(|x| x.def.id == f.id));
}

/// Rewritten for Task 13B: `ingest.rs` documents that it deliberately replaced
/// an `ON CONFLICT DO UPDATE` (silent overwrite) with close-and-insert --
/// exactly the "never overwrite an observation" global constraint this branch
/// exists to enforce. The old assertion here counted every row for
/// (instrument, field) with no `system_to` filter, which was correct only
/// under the old overwrite-in-place behaviour: a second ingest with a changed
/// value now leaves TWO rows on disk (one closed, one current), not one. The
/// intent -- "ingesting the same fact twice converges to one current value,
/// never a duplicate" -- is unchanged; the assertion is rewritten to check
/// the current row only, and to assert the superseded row survives instead
/// of being silently overwritten.
#[tokio::test]
#[ignore = "requires postgres"]
async fn ingest_twice_converges_no_duplicates() {
    use getbloomdata_lib::{fields, ingest, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let class = getbloomdata_lib::registry::create_asset_class(&pool, &uniq("EquityT9"), "t")
        .await
        .unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST_T9", "px", "numeric", None, None, "", false, None, None)
        .await
        .unwrap();
    let entry = add_book_entry(&pool, class.id, "T9", &format!("{} US", uniq("T9"))).await;
    let v = views::create_view(&pool, &uniq("t9view"), "").await.unwrap();
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ingesting') RETURNING id",
    )
    .bind(v.id)
    .fetch_one(&pool)
    .await
    .unwrap();

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
    let mk = |val: f64| FetchOutcome {
        cells: vec![ObsCell {
            instrument_id: entry.instrument_id,
            field_id: f.id,
            obs_date: d,
            value: CellValue::Num(val),
        }],
        problems: vec![],
    };
    ingest::ingest_outcome(&pool, run_id, &mk(100.0)).await.unwrap();
    ingest::ingest_outcome(&pool, run_id, &mk(101.5)).await.unwrap(); // re-run: converges, not dup

    // Never a duplicate CURRENT row for the same logical series.
    let (count, val): (i64, f64) = sqlx::query_as(
        "SELECT count(*)::bigint, max(value_num)
         FROM observation
         WHERE instrument_id = $1 AND field_id = $2 AND system_to = 'infinity'",
    )
    .bind(entry.instrument_id)
    .bind(f.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(count, 1);
    assert_eq!(val, 101.5);

    // The corrected value is superseded, never overwritten: the append-only
    // history stays on disk under a closed system_to.
    let (superseded,): (i64,) = sqlx::query_as(
        "SELECT count(*)::bigint FROM observation
         WHERE instrument_id = $1 AND field_id = $2 AND system_to <> 'infinity'",
    )
    .bind(entry.instrument_id)
    .bind(f.id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(superseded, 1,
               "a corrected value must close system_to on the old row, never overwrite it");
}

// A fetcher that returns canned data. This is the payoff of the reshaped
// `DataFetcher` trait (spec A2 §2.4): the orchestrator's whole path is now
// exercisable with no Bloomberg, no Excel, and no network.
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

/// Creates a class + numeric field + book entry + view, returns
/// (view_id, instrument_id, field_id).
async fn seed_view(pool: &sqlx::PgPool) -> (i64, i64, i64) {
    use getbloomdata_lib::{fields, views};
    let class = getbloomdata_lib::registry::create_asset_class(pool, &uniq("EquityE2E"), "t")
        .await
        .unwrap();
    let f = fields::create_field(pool, class.id, "PX_LAST", "px", "numeric", None, None, "", false, None, None)
        .await
        .unwrap();
    let entry = add_book_entry(pool, class.id, "E2E", &format!("{} US", uniq("E2E"))).await;
    let v = views::create_view(pool, &uniq("e2eview"), "").await.unwrap();
    views::set_view_instruments(pool, v.id, &[entry.instrument_id]).await.unwrap();
    (v.id, entry.instrument_id, f.id)
}

fn mock_cfg(dir: &std::path::Path) -> orchestrator::PipelineConfig {
    orchestrator::PipelineConfig {
        data_dir: dir.to_path_buf(),
        python_path: std::path::PathBuf::from("python"),
        script_path: std::path::PathBuf::from("scripts/blp_fetch.py"),
        request_timeout_s: 60,
        soft_limit: 100_000,
        blp_host: None,
        blp_port: None,
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn eod_pipeline_ingests_and_ends_ok() {
    use getbloomdata_lib::{db, orchestrator};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let (view_id, instrument_id, field_id) = seed_view(&pool).await;

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
    let fetcher = MockFetcher {
        cells: vec![ObsCell { instrument_id, field_id, obs_date: d,
                              value: CellValue::Num(305.59) }],
        problems: vec![],
        fail: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let cfg = mock_cfg(dir.path());
    let out = orchestrator::run_eod_with(&pool, &cfg, &fetcher, view_id, "manual", d, false)
        .await.unwrap();

    match out {
        orchestrator::RunOutcome::Completed { run_id, summary, .. } => {
            assert_eq!(summary.inserted, 1);
            assert_eq!(summary.issues, 0);
            let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "ok");

            // The observation actually landed, with the right value and date.
            let v: f64 = sqlx::query_scalar(
                "SELECT value_num FROM observation
                 WHERE instrument_id=$1 AND field_id=$2 AND obs_date=$3")
                .bind(instrument_id).bind(field_id).bind(d)
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
    let (view_id, instrument_id, field_id) = seed_view(&pool).await;

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
    // The holiday shape: no data, one problem, nothing to ingest.
    let fetcher = MockFetcher {
        cells: vec![],
        problems: vec![CellProblem {
            instrument_id: Some(instrument_id), field_id: Some(field_id), obs_date: Some(d),
            code: "no_data".into(), detail: "no trading day returned".into() }],
        fail: false,
    };
    let dir = tempfile::tempdir().unwrap();
    let out = orchestrator::run_eod_with(&pool, &mock_cfg(dir.path()), &fetcher,
                                         view_id, "scheduled", d, false).await.unwrap();
    match out {
        orchestrator::RunOutcome::Completed { run_id, summary, .. } => {
            assert_eq!(summary.inserted, 0);
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
    assert_eq!(first, second); // restart must not re-roll
    let win_s = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let win_e = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    assert!(first >= win_s && first < win_e);
}

// ---------------------------------------------------------------------------
// End-to-end smoke test (design §8, plan task 15)
//
// The one test that talks to the real Bloomberg Terminal. Idempotent by
// design: fixtures are looked up before being created through the real
// `book::add` resolution path -- a hand-seeded instrument would test nothing
// here, since the whole point of this test is exercising the real wire.
// ---------------------------------------------------------------------------

const SMOKE_CLASS: &str = "SmokeEquity";
const SMOKE_SECURITY: &str = "AAPL US Equity";
const SMOKE_VIEW: &str = "smoke-view";

#[tokio::test]
#[ignore = "requires postgres AND a live Bloomberg Terminal"]
async fn smoke_real_bloomberg_end_to_end() {
    use getbloomdata_lib::book::{self, AddOutcome, AddToBook};
    use getbloomdata_lib::master_fetch::BlpapiMasterFetcher;
    use getbloomdata_lib::resolution::score::Hints;
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
            fields::create_field(&pool, class_id, mnemonic, mnemonic, kind, None, None, "", false, None, None)
                .await.unwrap();
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let cfg = orchestrator::PipelineConfig {
        data_dir: dir.path().to_path_buf(),
        python_path: std::path::PathBuf::from("python"),
        script_path: std::path::PathBuf::from("scripts/blp_fetch.py"),
        request_timeout_s: 120,
        soft_limit: 100_000,
        blp_host: None,
        blp_port: None,
    };

    let instrument_id: i64 = match sqlx::query_scalar::<_, i64>(
        "SELECT b.instrument_id FROM book_entry b
           JOIN instrument_alias a ON a.instrument_id = b.instrument_id
          WHERE a.id_type = 'bdp_security' AND a.value = $1 AND a.system_to = 'infinity'")
        .bind(SMOKE_SECURITY).fetch_optional(&pool).await.unwrap() {
        Some(id) => id,
        None => {
            let fetcher = BlpapiMasterFetcher { cfg: &cfg, pool: &pool };
            let req = AddToBook {
                raw: "AAPL US".into(), yellow_key: "Equity".into(),
                asset_class_id: class_id, label: "Apple".into(), hints: Hints::default(),
            };
            match book::add(&pool, &fetcher, &req, "smoke").await.expect("book::add failed") {
                AddOutcome::Added(entry) => entry.instrument_id,
                other => panic!("expected Added, got {other:?}"),
            }
        }
    };
    let view_id: i64 = match sqlx::query_scalar("SELECT id FROM view WHERE name=$1")
        .bind(SMOKE_VIEW).fetch_optional(&pool).await.unwrap() {
        Some(id) => id,
        None => views::create_view(&pool, SMOKE_VIEW, "smoke").await.unwrap().id,
    };
    views::set_view_instruments(&pool, view_id, &[instrument_id]).await.unwrap();

    // --- the real thing: previous trading day, real BLPAPI, real database ---
    let obs_date = scheduler::previous_weekday(chrono::Local::now().date_naive());
    let out = orchestrator::run_eod(&pool, &cfg, view_id, "manual", obs_date, true)
        .await.expect("live BLPAPI run failed");

    let orchestrator::RunOutcome::Completed { run_id, summary, .. } = out else {
        panic!("expected Completed, got {out:?}");
    };
    eprintln!("smoke: run {run_id} obs_date={obs_date} \
               upserted={} issues={}", summary.inserted, summary.issues);

    // Both fields came back: PX_LAST via HistoricalDataRequest, NAME via
    // ReferenceDataRequest, both stamped with the same previous-day obs_date.
    let px: f64 = sqlx::query_scalar(
        "SELECT o.value_num FROM observation o JOIN field_def f ON f.id=o.field_id
         WHERE o.instrument_id=$1 AND f.mnemonic='PX_LAST' AND o.obs_date=$2")
        .bind(instrument_id).bind(obs_date).fetch_one(&pool).await
        .expect("no PX_LAST observation ingested");
    assert!(px > 0.0, "implausible price {px}");

    let name: String = sqlx::query_scalar(
        "SELECT o.value_text FROM observation o JOIN field_def f ON f.id=o.field_id
         WHERE o.instrument_id=$1 AND f.mnemonic='NAME' AND o.obs_date=$2")
        .bind(instrument_id).bind(obs_date).fetch_one(&pool).await
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

#[tokio::test]
#[ignore = "requires postgres"]
async fn describe_deletion_counts_what_hangs_off_an_asset() {
    use getbloomdata_lib::deletion::{describe_deletion, EntityKind};
    use getbloomdata_lib::{fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("DescribeCls"), "test").await.unwrap();
    let entry = add_book_entry(&pool, class.id, "Describe Me", &format!("{} US", uniq("DSC"))).await;
    let field = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric", None, None, "", false, None, None)
        .await.unwrap();
    let view = views::create_view(&pool, &uniq("DescribeView"), "").await.unwrap();
    views::set_view_instruments(&pool, view.id, &[entry.instrument_id]).await.unwrap();

    let run_id = new_run(&pool, view.id).await;
    let basis = raw_basis_id(&pool).await;
    sqlx::query(
        "INSERT INTO observation (instrument_id, field_id, obs_date, value_num, basis_id, layer, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $4, 'raw', $3),
                ($1, $2, DATE '2026-08-11', 2.0, $4, 'raw', $3)")
        .bind(entry.instrument_id).bind(field.id).bind(run_id).bind(basis)
        .execute(&pool).await.unwrap();

    let impact = describe_deletion(&pool, EntityKind::Asset, entry.instrument_id).await.unwrap();
    assert_eq!(impact.label, "Describe Me");
    assert_eq!(impact.observations, 2);
    assert_eq!(impact.first_obs, Some("2026-08-10".parse().unwrap()));
    assert_eq!(impact.last_obs, Some("2026-08-11".parse().unwrap()));
    assert_eq!(impact.views, 1);
    assert!(impact.can_retire && impact.can_purge);
    assert!(impact.blocked_reason.is_none());
    // New in Task 13B's purge semantics: an asset purge never deletes
    // observations, so the dialog must be told to word it that way.
    assert!(impact.purge_keeps_history,
            "an asset purge must be described as history-preserving under the new contract");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn describe_deletion_blocks_a_non_empty_asset_class() {
    use getbloomdata_lib::deletion::{describe_deletion, EntityKind};
    use getbloomdata_lib::registry;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("BlockedCls"), "test").await.unwrap();
    add_book_entry(&pool, class.id, "Occupant", &format!("{} US", uniq("OCC"))).await;

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
    use getbloomdata_lib::deletion::{delete_asset_class, delete_book_entry, DeleteMode};
    use getbloomdata_lib::error::AppError;
    use getbloomdata_lib::registry;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("EmptyMeCls"), "test").await.unwrap();
    let entry = add_book_entry(&pool, class.id, "Tenant", &format!("{} US", uniq("TEN"))).await;

    let err = delete_asset_class(&pool, class.id).await.unwrap_err();
    assert!(matches!(err, AppError::DeleteBlocked { .. }),
            "expected DeleteBlocked, got {err:?}");

    // Purging the occupying book entry leaves identity (the instrument) intact
    // but frees the class -- a class is empty of BOOK ENTRIES, not of every
    // instrument that ever cited it.
    delete_book_entry(&pool, entry.instrument_id, DeleteMode::Purge).await.unwrap();
    delete_asset_class(&pool, class.id).await.unwrap();

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset_class WHERE id = $1")
        .bind(class.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn retiring_an_asset_hides_it_from_views_but_keeps_its_observations() {
    use getbloomdata_lib::deletion::{delete_book_entry, DeleteMode};
    use getbloomdata_lib::{fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("RetireCls"), "test").await.unwrap();
    let entry = add_book_entry(&pool, class.id, "Retiree", &format!("{} US", uniq("RET"))).await;
    let field = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric", None, None, "", false, None, None)
        .await.unwrap();
    let view = views::create_view(&pool, &uniq("RetireView"), "").await.unwrap();
    views::set_view_instruments(&pool, view.id, &[entry.instrument_id]).await.unwrap();
    let run_id = new_run(&pool, view.id).await;
    let basis = raw_basis_id(&pool).await;
    sqlx::query(
        "INSERT INTO observation (instrument_id, field_id, obs_date, value_num, basis_id, layer, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $4, 'raw', $3)")
        .bind(entry.instrument_id).bind(field.id).bind(run_id).bind(basis)
        .execute(&pool).await.unwrap();

    assert_eq!(views::view_instruments(&pool, view.id).await.unwrap().len(), 1);

    delete_book_entry(&pool, entry.instrument_id, DeleteMode::Retire).await.unwrap();

    assert!(views::view_instruments(&pool, view.id).await.unwrap().is_empty(),
            "a retired asset must drop out of view resolution");
    let (obs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM observation WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(obs, 1, "retire never destroys collected data");
    let (row,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(row, 1);
}

/// Rewritten for Task 13B: purge semantics changed. This test used to assert
/// that purging an asset deleted its observations -- that is no longer true,
/// and porting the old assertion unchanged would have re-encoded a guarantee
/// this branch deliberately removed.
///
/// `purge_book_entry_tx` (deletion.rs) now removes ONLY `book_entry` and its
/// `view_instrument` memberships. The instrument, its aliases, its
/// observations and its ingest issues all survive: identity and history are
/// not the user's to destroy by removing something from their list. This is
/// exactly what `DeletionImpact::purge_keeps_history` (see
/// `describe_deletion_counts_what_hangs_off_an_asset` above) exists to tell
/// the confirm dialog. The rewrite below asserts the new contract explicitly,
/// including that observations and aliases DO survive.
#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_a_book_entry_removes_only_the_book_entry_and_its_memberships() {
    use getbloomdata_lib::deletion::{delete_book_entry, DeleteMode};
    use getbloomdata_lib::instrument::store;
    use getbloomdata_lib::{fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("PurgeCls"), "test").await.unwrap();
    let entry = add_book_entry(&pool, class.id, "Doomed", &format!("{} US", uniq("DOOM"))).await;
    let field = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric", None, None, "", false, None, None)
        .await.unwrap();
    let view = views::create_view(&pool, &uniq("PurgeView"), "").await.unwrap();
    views::set_view_instruments(&pool, view.id, &[entry.instrument_id]).await.unwrap();

    let run_id = new_run(&pool, view.id).await;
    sqlx::query("INSERT INTO hit_ledger (run_id, estimated_hits) VALUES ($1, 7)")
        .bind(run_id).execute(&pool).await.unwrap();
    let basis = raw_basis_id(&pool).await;
    sqlx::query(
        "INSERT INTO observation (instrument_id, field_id, obs_date, value_num, basis_id, layer, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $4, 'raw', $3)")
        .bind(entry.instrument_id).bind(field.id).bind(run_id).bind(basis)
        .execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, instrument_id, field_id, obs_date, severity, code, detail)
         VALUES ($1, $2, $3, DATE '2026-08-10', 'warn', 'no_data', 'test')")
        .bind(run_id).bind(entry.instrument_id).bind(field.id).execute(&pool).await.unwrap();

    let aliases_before = store::aliases(&pool, entry.instrument_id).await.unwrap().len();
    assert!(aliases_before > 0, "fixture must actually carry an identity to prove it survives");

    delete_book_entry(&pool, entry.instrument_id, DeleteMode::Purge).await.unwrap();

    // What purge DOES remove: the book entry and its view membership.
    for (table, col) in [("view_instrument", "instrument_id"), ("book_entry", "instrument_id")] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE {col} = $1"))
            .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should have no rows for the purged book entry");
    }

    // What purge must NEVER touch: identity and everything hanging off it.
    let (instrument_rows,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM instrument WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(instrument_rows, 1, "the instrument's identity must survive a book purge");
    let aliases_after = store::aliases(&pool, entry.instrument_id).await.unwrap().len();
    assert_eq!(aliases_after, aliases_before, "aliases must survive a book purge");
    let (obs,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM observation WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(obs, 1, "observations must survive a book purge -- history is not the user's \
                        to destroy by removing something from their list");
    let (issues,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM ingest_issue WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(issues, 1, "ingest issues must survive a book purge too");

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
    use getbloomdata_lib::{fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("FldPurgeCls"), "test").await.unwrap();
    let entry = add_book_entry(&pool, class.id, "Holder", &format!("{} US", uniq("HLD"))).await;
    let field = fields::create_field(&pool, class.id, "PX_VOLUME", "Volume", "numeric", None, None, "", false, None, None)
        .await.unwrap();
    let view = views::create_view(&pool, &uniq("FldPurgeView"), "").await.unwrap();
    views::set_view_fields(&pool, view.id, &[field.id]).await.unwrap();
    let run_id = new_run(&pool, view.id).await;
    let basis = raw_basis_id(&pool).await;
    sqlx::query(
        "INSERT INTO observation (instrument_id, field_id, obs_date, value_num, basis_id, layer, run_id)
         VALUES ($1, $2, DATE '2026-08-10', 5.0, $4, 'raw', $3)")
        .bind(entry.instrument_id).bind(field.id).bind(run_id).bind(basis)
        .execute(&pool).await.unwrap();

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
    use getbloomdata_lib::{registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = registry::create_asset_class(&pool, &uniq("VPurgeCls"), "test").await.unwrap();
    let entry = add_book_entry(&pool, class.id, "Member", &format!("{} US", uniq("MEM"))).await;
    let view = views::create_view(&pool, &uniq("FreshView"), "").await.unwrap();
    views::set_view_instruments(&pool, view.id, &[entry.instrument_id]).await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '18:00', TIME '19:00', true)")
        .bind(view.id).execute(&pool).await.unwrap();

    delete_view(&pool, view.id, DeleteMode::Purge).await.unwrap();

    for table in ["schedule", "view_instrument", "view_field"] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE view_id = $1"))
            .bind(view.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should be empty for the purged view");
    }
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM view WHERE id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
    // The book entry itself is untouched: a view is a selection, not an owner.
    let (b,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(b, 1);
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
    assert!(!due.iter().any(|(_, vid, ..)| *vid == view.id),
            "a retired view must not appear in the due list");
}

/// The bulk import reasons about the WHOLE registry, so it cannot be tested
/// against the shared fixture database: those tests never clean up and run on
/// parallel threads, so an instrument another test inserted mid-flight would
/// show up here as a proposed removal -- and, with Retire as the default,
/// this test would deactivate another test's fixture. These tests get their
/// own database, wiped at the start of each, and a mutex so they never
/// overlap -- the counts below are safe against that isolated, single-writer
/// database, unlike a global count against the shared `bloom_test` pool the
/// rest of this file uses.
static BULK_DB: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn bulk_pool() -> sqlx::PgPool {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let (base, _) = url.rsplit_once('/').expect("database url ends with /<dbname>");
    let admin = sqlx::PgPool::connect(&format!("{base}/postgres")).await.unwrap();
    let exists: Option<(i32,)> = sqlx::query_as(
        "SELECT 1 FROM pg_database WHERE datname = 'bloom_test_bulk'")
        .fetch_optional(&admin).await.unwrap();
    if exists.is_none() {
        sqlx::query("CREATE DATABASE bloom_test_bulk").execute(&admin).await.unwrap();
    }
    admin.close().await;
    let pool = getbloomdata_lib::db::connect(&format!("{base}/bloom_test_bulk"))
        .await.unwrap();
    // CASCADE is transitive: truncating these three root tables also clears
    // every table with a (possibly indirect) foreign key onto one of them --
    // book_entry, instrument_alias, instrument_attr, instrument_candidate,
    // instrument_link, resolution_decision/review, view_instrument,
    // view_field, schedule, run, observation, ingest_issue, hit_ledger.
    // `adjustment_basis` is deliberately NOT truncated: it holds the seed
    // rows (RAW / LEGACY_DPDF) `ingest_outcome` depends on, and nothing here
    // references it in the direction CASCADE would follow.
    sqlx::query("TRUNCATE instrument, asset_class, view RESTART IDENTITY CASCADE")
        .execute(&pool).await.unwrap();
    pool
}

/// Build a book entry via `book::add` against the bulk database, returning
/// just the instrument_id -- the shape the bulk tests below want.
async fn mk_book_entry(pool: &sqlx::PgPool, class_id: i64, label: &str, ticker: &str) -> i64 {
    add_book_entry(pool, class_id, label, ticker).await.instrument_id
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn export_then_import_is_a_no_op() {
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test")
        .await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, "Daily", "").await.unwrap();
    let a = mk_book_entry(&pool, class.id, "Round Trip", "RTP US").await;
    let b = mk_book_entry(&pool, class.id, "Second", "SEC US").await;
    getbloomdata_lib::views::set_view_instruments(&pool, view.id, &[a]).await.unwrap();
    getbloomdata_lib::book::set_active(&pool, b, false).await.unwrap();

    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();
    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert!(plan.invalid_rows.is_empty(), "invalid rows: {:?}", plan.invalid_rows);
    assert!(plan.is_empty(), "a fresh round trip must change nothing, got {plan:?}");
    // A retired asset must survive the round trip retired, not be reactivated.
    assert!(plan.reactivations.is_empty());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_adds_edits_and_purges_in_one_transaction() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::{read_assets_sheet, write_assets_sheet, ExportRow};
    use getbloomdata_lib::deletion::DeleteMode;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test")
        .await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, "Daily", "").await.unwrap();
    let keep = mk_book_entry(&pool, class.id, "Keep", "KEP US").await;
    let drop = mk_book_entry(&pool, class.id, "Drop", "DRP US").await;
    let third = mk_book_entry(&pool, class.id, "Third", "THR US").await;
    getbloomdata_lib::views::set_view_instruments(&pool, view.id, &[third]).await.unwrap();

    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    // Rewrite the sheet the way a user would in Excel: rename `keep`, put it in
    // the Daily view, add a row with a blank id, and delete `drop`'s row.
    let data = read_assets_sheet(&path).unwrap();
    let mut rows: Vec<ExportRow> = data.rows.iter()
        .filter(|r| r.instrument_id != Some(drop))
        .map(|r| ExportRow {
            instrument_id: r.instrument_id.unwrap_or(0),
            label: if r.instrument_id == Some(keep) { "Keep Renamed".into() } else { r.label.clone() },
            class: r.class.clone(), identifier: r.identifier.clone(),
            yellow_key: r.yellow_key.clone(), active: r.active,
            security: String::new(), status: String::new(),
            views: if r.instrument_id == Some(keep) { vec!["Daily".into()] } else { r.views.clone() },
        }).collect();
    rows.push(ExportRow {
        instrument_id: 0, label: "Brand New".into(), class: "Equity".into(),
        identifier: "NEW US".into(), yellow_key: "Equity".into(),
        active: true, security: String::new(), status: String::new(), views: vec![],
    });
    write_assets_sheet(&path, &rows, &["Daily".to_string()], &["Equity".to_string()])
        .unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert!(plan.invalid_rows.is_empty(), "invalid rows: {:?}", plan.invalid_rows);
    assert_eq!(plan.adds.len(), 1);
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(plan.edits[0].id, keep);
    assert_eq!(plan.membership_changes.len(), 1);
    assert_eq!(plan.removals.len(), 1);
    assert_eq!(plan.removals[0].id, drop);
    assert!(!plan.requires_typed_confirmation, "1 of 3 active is not over half");

    let fetcher = MockMasterFetcher {
        identity_raw: identity_raw_for("NEW US Equity"), ..Default::default()
    };
    let res = apply_import_with(
        &pool, &fetcher, &path, &plan.file_hash, &[(drop, DeleteMode::Purge)], None)
        .await.unwrap();
    assert_eq!(res.added, 1);
    assert_eq!(res.edited, 1);
    assert_eq!(res.removed, 1);
    assert!(res.workbook_refreshed, "the happy path must refresh the workbook on disk");

    let (gone,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE instrument_id = $1")
        .bind(drop).fetch_one(&pool).await.unwrap();
    assert_eq!(gone, 0, "purge deletes the book entry outright");
    let (label,): (String,) = sqlx::query_as("SELECT label FROM book_entry WHERE instrument_id = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(label, "Keep Renamed");
    let (members,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM view_instrument WHERE view_id = $1 AND instrument_id = $2")
        .bind(view.id).bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(members, 1, "the x in the Daily column added a membership");
    let (still,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE instrument_id = $1")
        .bind(third).fetch_one(&pool).await.unwrap();
    assert_eq!(still, 1, "an untouched row must survive");
    let (added,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM book_entry b JOIN instrument_alias a
           ON a.instrument_id = b.instrument_id
         WHERE a.id_type = 'bdp_security' AND a.value = 'NEW US Equity'
           AND a.system_to = 'infinity'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(added, 1);

    // `apply_import_with` overwrote the workbook at `path` with a fresh export
    // as part of landing the commit (see `workbook_refreshed` above), so this
    // is no longer testing the same sheet against a moved database -- it is
    // testing that the refreshed file is itself consistent with the database
    // it was just applied against: re-previewing it must find nothing left
    // to do, the same guarantee `export_then_import_is_a_no_op` checks for a
    // plain export, now checked for the post-apply re-export path too.
    let replay = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert!(replay.is_empty(), "the refreshed workbook must match the database, got {replay:?}");
}

/// Finding I5: a hand-built sheet (no `instrument_id` column) has no ids to
/// write back and, per guardrail 1, can never propose a removal -- so
/// overwriting it with a full registry export is not a service to the user,
/// it is destroying a file they wrote by hand (their own column order, their
/// own extra notes, whatever they pasted). The apply must skip the rewrite
/// entirely for such a sheet: the file on disk must be untouched, byte for
/// byte, and `workbook_refreshed` must honestly report `false`, even though
/// the database changes land exactly as they would for an export-produced
/// sheet.
#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_from_a_hand_built_sheet_leaves_the_file_untouched() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::SHEET_NAME;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("pasted.xlsx");

    getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test").await.unwrap();

    // Built the way a user would in Excel: no `instrument_id` column at all,
    // only the columns they bothered to fill in.
    let mut book = rust_xlsxwriter::Workbook::new();
    let s = book.add_worksheet();
    s.set_name(SHEET_NAME).unwrap();
    for (c, h) in ["label", "class", "identifier", "yellow_key"].iter().enumerate() {
        s.write_string(0, c as u16, *h).unwrap();
    }
    s.write_string(1, 0, "Hand Built").unwrap();
    s.write_string(1, 1, "Equity").unwrap();
    s.write_string(1, 2, "HB US").unwrap();
    s.write_string(1, 3, "Equity").unwrap();
    book.save(&path).unwrap();

    let before_bytes = std::fs::read(&path).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert!(!plan.has_id_column, "a hand-built sheet has no id column");
    assert!(plan.invalid_rows.is_empty(), "invalid rows: {:?}", plan.invalid_rows);
    assert_eq!(plan.adds.len(), 1);

    let fetcher = MockMasterFetcher {
        identity_raw: identity_raw_for("HB US Equity"), ..Default::default()
    };
    let res = apply_import_with(&pool, &fetcher, &path, &plan.file_hash, &[], None)
        .await.unwrap();
    assert!(!res.workbook_refreshed,
            "a hand-built sheet must not be overwritten by the post-apply export");

    let after_bytes = std::fs::read(&path).unwrap();
    assert_eq!(before_bytes, after_bytes,
               "the user's own file must survive an apply byte-for-byte untouched");

    let (added,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM book_entry b JOIN instrument_alias a
           ON a.instrument_id = b.instrument_id
         WHERE a.id_type = 'bdp_security' AND a.value = 'HB US Equity'
           AND a.system_to = 'infinity'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(added, 1, "the database changes must still land even though the file did not move");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_refuses_a_stale_hash() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::error::AppError;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();
    let fetcher = MockMasterFetcher::default();
    let err = apply_import_with(
        &pool, &fetcher, &path, "0000000000000000000000000000000000000000000000000000000000000000",
        &[], None).await.unwrap_err();
    assert!(matches!(err, AppError::ImportRejected { .. }), "got {err:?}");
}

/// Fix round 1, item 2 (happy path): a blank-id add row gets a real id
/// assigned by the commit, and the post-commit refresh must write that real
/// id back to the file -- not leave the blank the sheet started with.
#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_refreshes_the_workbook_with_the_newly_assigned_id() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::{read_assets_sheet, write_assets_sheet, ExportRow};
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test").await.unwrap();
    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    let rows = vec![ExportRow {
        instrument_id: 0, label: "Brand New".into(), class: "Equity".into(),
        identifier: "NEW US".into(), yellow_key: "Equity".into(),
        active: true, security: String::new(), status: String::new(), views: vec![],
    }];
    write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    let fetcher = MockMasterFetcher {
        identity_raw: identity_raw_for("NEW US Equity"), ..Default::default()
    };
    let res = apply_import_with(&pool, &fetcher, &path, &plan.file_hash, &[], None)
        .await.unwrap();
    assert!(res.workbook_refreshed, "the happy path must report a successful refresh");

    let refreshed = read_assets_sheet(&path).unwrap();
    let new_row = refreshed.rows.iter().find(|r| r.label == "Brand New")
        .expect("the new row must still be on the refreshed sheet");
    assert!(new_row.instrument_id.is_some(),
            "the refreshed file must carry the database's assigned id, not the blank it started with");
}

/// Fix round 1, items 1 and 2 (failure path): the reviewer reproduced this
/// against a real database by making the workbook read-only before apply --
/// simulating Excel's own sharing-violation lock, `os error 5` on Windows --
/// and found the committed purge and add both landed while the caller was
/// told the whole call failed. A failed refresh must never demote a landed
/// commit to an `Err`: the transaction already committed, so the caller must
/// be told it succeeded, with `workbook_refreshed` carrying the honest bad
/// news about the file instead.
#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_succeeds_even_when_the_workbook_refresh_is_locked_out() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::{read_assets_sheet, write_assets_sheet, ExportRow};
    use getbloomdata_lib::deletion::DeleteMode;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test")
        .await.unwrap();
    let drop = mk_book_entry(&pool, class.id, "Drop", "DRP US").await;
    // A second, untouched asset so this one removal is not "more than half the
    // active book" -- guardrail 2 is a different mechanism from the one this
    // test is about, and must not fire here.
    mk_book_entry(&pool, class.id, "Untouched", "UNT US").await;
    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    // Rewrite the sheet the way the earlier apply-path tests do: drop's row is
    // gone (a removal) and a blank-id row proposes an add, in the same call.
    let data = read_assets_sheet(&path).unwrap();
    let mut rows: Vec<ExportRow> = data.rows.iter()
        .filter(|r| r.instrument_id != Some(drop))
        .map(|r| ExportRow {
            instrument_id: r.instrument_id.unwrap_or(0), label: r.label.clone(), class: r.class.clone(),
            identifier: r.identifier.clone(), yellow_key: r.yellow_key.clone(), active: r.active,
            security: String::new(), status: String::new(), views: r.views.clone(),
        }).collect();
    rows.push(ExportRow {
        instrument_id: 0, label: "Brand New".into(), class: "Equity".into(),
        identifier: "NEW US".into(), yellow_key: "Equity".into(),
        active: true, security: String::new(), status: String::new(), views: vec![],
    });
    write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();

    // Simulate the workbook being locked/unwritable, portably, WITHOUT
    // breaking the read `apply_import_with` does of its own accord before
    // it ever touches the transaction: its very first line re-reads and
    // re-hashes `path` for an integrity check (`sheet::file_sha256`), and
    // `preview_import` above already needed a real read too. So whatever
    // we do here must still let the file be *read*; only the post-commit
    // re-export's *write* may fail.
    //
    // (A first attempt replaced the target file outright with a directory,
    // matching a literal reading of "make rename fail on every OS" -- but
    // that also makes `file_sha256`'s read fail, so `apply_import_with`
    // returns Err from its top-of-function integrity check before the
    // transaction even runs. That is exactly the outcome this test exists
    // to rule out: a failed refresh demoting a landed commit to an Err.
    // Confirmed locally: `outcome.expect(..)` panicked on `Io(PermissionDenied)`
    // with zero rows committed, never reaching the `workbook_refreshed`
    // assertion below.)
    //
    // The mechanism instead differs by OS because "block only the write"
    // does too. On Windows, the read-only FILE_ATTRIBUTE on the target
    // itself blocks `rename`-over-target -- the same sharing violation
    // Excel's own file lock produces -- while leaving reads of that file
    // unaffected. On POSIX, `rename(2)` does not consult the destination
    // file's permission bits at all -- only write permission on the
    // *directory* the destination lives in controls whether it can be
    // replaced -- so the readonly-file trick silently no-ops on Linux CI
    // (the temp-then-rename write goes through, and `workbook_refreshed`
    // ends up honestly, but wrongly, `true`). Removing write permission
    // from the containing directory is the real POSIX equivalent: reads
    // only need read+search on the directory, so `file_sha256` and the
    // already-computed `plan` are unaffected; only creating the temp file
    // and renaming over the target are blocked.
    #[cfg(windows)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o555);
        std::fs::set_permissions(dir.path(), perms).unwrap();
    }

    let fetcher = MockMasterFetcher {
        identity_raw: identity_raw_for("NEW US Equity"), ..Default::default()
    };
    let outcome = apply_import_with(
        &pool, &fetcher, &path, &plan.file_hash, &[(drop, DeleteMode::Purge)], None).await;

    // Restore write access unconditionally, before any assertion that might
    // panic, so the tempdir can still clean itself up.
    #[cfg(windows)]
    {
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&path, perms).unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(dir.path()).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(dir.path(), perms).unwrap();
    }

    let res = outcome.expect("a failed workbook refresh must not demote a committed apply to an error");
    assert!(!res.workbook_refreshed, "the refresh genuinely failed and must say so honestly");

    let (gone,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE instrument_id = $1")
        .bind(drop).fetch_one(&pool).await.unwrap();
    assert_eq!(gone, 0, "the committed purge must have landed despite the refresh failure");
    let (added,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM book_entry b JOIN instrument_alias a
           ON a.instrument_id = b.instrument_id
         WHERE a.id_type = 'bdp_security' AND a.value = 'NEW US Equity'
           AND a.system_to = 'infinity'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(added, 1, "the committed add must have landed despite the refresh failure");
}

/// Fix round 1, item 3: an asset the sheet no longer mentions is a removal
/// whether the user meant it or not -- e.g. another writer changed the
/// registry between preview and apply. Nothing may be applied against a
/// removal the caller never actually reviewed, so an empty `removal_modes`
/// must refuse the whole call rather than silently default to Retire.
#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_refuses_a_removal_the_caller_never_reviewed() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::{read_assets_sheet, write_assets_sheet, ExportRow};
    use getbloomdata_lib::error::AppError;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test")
        .await.unwrap();
    let keep = mk_book_entry(&pool, class.id, "Keep", "KEP US").await;
    let drop = mk_book_entry(&pool, class.id, "Drop", "DRP US").await;
    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    let data = read_assets_sheet(&path).unwrap();
    let rows: Vec<ExportRow> = data.rows.iter()
        .filter(|r| r.instrument_id != Some(drop))
        .map(|r| ExportRow {
            instrument_id: r.instrument_id.unwrap_or(0), label: r.label.clone(), class: r.class.clone(),
            identifier: r.identifier.clone(), yellow_key: r.yellow_key.clone(), active: r.active,
            security: String::new(), status: String::new(), views: r.views.clone(),
        }).collect();
    write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert_eq!(plan.removals.len(), 1);
    assert_eq!(plan.removals[0].id, drop);
    assert!(!plan.requires_typed_confirmation, "1 of 2 active is not over half");

    let fetcher = MockMasterFetcher::default();
    let err = apply_import_with(&pool, &fetcher, &path, &plan.file_hash, &[], None)
        .await.unwrap_err();
    let AppError::ImportRejected { reason } = &err else {
        panic!("an unreviewed removal must be refused with ImportRejected, got {err:?}");
    };
    assert!(reason.contains("1 removal"),
            "the refusal must say how many removals were not reviewed, got {reason:?}");

    let (still_active,): (bool,) = sqlx::query_as("SELECT active FROM book_entry WHERE instrument_id = $1")
        .bind(drop).fetch_one(&pool).await.unwrap();
    assert!(still_active, "an unreviewed removal must not be silently retired");
    let (label,): (String,) = sqlx::query_as("SELECT label FROM book_entry WHERE instrument_id = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(label, "Keep", "nothing else may move either -- this is an all-or-nothing refusal");
}

/// Fix round 2, item 2: `removal_modes` naming the same asset twice with
/// conflicting intent must be refused outright, not resolved by slice order.
/// Collecting straight into a `HashMap` would let `(id, Purge)` silently win
/// or lose against an earlier `(id, Retire)` depending only on which came
/// last in the slice -- exactly the kind of silent decision a destructive
/// action must never make on the caller's behalf.
#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_refuses_conflicting_duplicate_removal_modes() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::{read_assets_sheet, write_assets_sheet, ExportRow};
    use getbloomdata_lib::deletion::DeleteMode;
    use getbloomdata_lib::error::AppError;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test")
        .await.unwrap();
    let keep = mk_book_entry(&pool, class.id, "Keep", "KEP US").await;
    let drop = mk_book_entry(&pool, class.id, "Drop", "DRP US").await;
    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    let data = read_assets_sheet(&path).unwrap();
    let rows: Vec<ExportRow> = data.rows.iter()
        .filter(|r| r.instrument_id != Some(drop))
        .map(|r| ExportRow {
            instrument_id: r.instrument_id.unwrap_or(0), label: r.label.clone(), class: r.class.clone(),
            identifier: r.identifier.clone(), yellow_key: r.yellow_key.clone(), active: r.active,
            security: String::new(), status: String::new(), views: r.views.clone(),
        }).collect();
    write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert_eq!(plan.removals.len(), 1);
    assert_eq!(plan.removals[0].id, drop);
    assert!(!plan.requires_typed_confirmation, "1 of 2 active is not over half");

    // Two entries for the SAME id, opposite destructive intent.
    let conflicting = [(drop, DeleteMode::Retire), (drop, DeleteMode::Purge)];
    let fetcher = MockMasterFetcher::default();
    let err = apply_import_with(
        &pool, &fetcher, &path, &plan.file_hash, &conflicting, None).await.unwrap_err();
    let AppError::ImportRejected { reason } = &err else {
        panic!("conflicting duplicate removal modes must be refused with ImportRejected, got {err:?}");
    };
    assert!(reason.contains(&drop.to_string()),
            "the refusal must name the duplicated asset id, got {reason:?}");

    // Nothing moved: neither retired nor purged.
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE instrument_id = $1 AND active")
        .bind(drop).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "a call refused for conflicting modes must leave the asset untouched");
    let (label,): (String,) = sqlx::query_as("SELECT label FROM book_entry WHERE instrument_id = $1")
        .bind(keep).fetch_one(&pool).await.unwrap();
    assert_eq!(label, "Keep");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_large_removal_set_needs_the_typed_count() {
    use getbloomdata_lib::bulk::apply_import_with;
    use getbloomdata_lib::bulk::sheet::{read_assets_sheet, write_assets_sheet, ExportRow};
    use getbloomdata_lib::error::AppError;
    use getbloomdata_lib::master_fetch::MockMasterFetcher;
    let _guard = BULK_DB.lock().await;
    let pool = bulk_pool().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(&pool, "Equity", "test")
        .await.unwrap();
    for i in 1..=4 {
        mk_book_entry(&pool, class.id, &format!("A{i}"), &format!("A{i} US")).await;
    }
    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    // Keep one row of four -- a truncated paste, which is exactly what
    // guardrail 2 exists to catch.
    let data = read_assets_sheet(&path).unwrap();
    let rows: Vec<ExportRow> = data.rows.iter().take(1).map(|r| ExportRow {
        instrument_id: r.instrument_id.unwrap_or(0), label: r.label.clone(), class: r.class.clone(),
        identifier: r.identifier.clone(), yellow_key: r.yellow_key.clone(), active: r.active,
        security: String::new(), status: String::new(), views: r.views.clone(),
    }).collect();
    write_assets_sheet(&path, &rows, &[], &["Equity".to_string()]).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert_eq!(plan.removals.len(), 3);
    assert!(plan.requires_typed_confirmation);

    let fetcher = MockMasterFetcher::default();
    let err = apply_import_with(
        &pool, &fetcher, &path, &plan.file_hash, &[], None).await.unwrap_err();
    let AppError::ImportRejected { reason } = &err else {
        panic!("an unconfirmed mass removal must be refused with ImportRejected, got {err:?}");
    };
    assert!(reason.contains("confirm the count"),
            "the refusal must explain that the count needs confirming, got {reason:?}");
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE active")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 4, "nothing may be applied when the guardrail refuses");

    // With the count typed AND every removal given an explicit mode, the same
    // call goes through. Retire is passed explicitly for all three: since the
    // last fix round, an id absent from `removal_modes` is refused outright
    // rather than silently defaulted, so this is no longer "the default" but
    // still exercises the same Retire-not-Purge behaviour end to end.
    let modes: Vec<(i64, getbloomdata_lib::deletion::DeleteMode)> = plan.removals.iter()
        .map(|r| (r.id, getbloomdata_lib::deletion::DeleteMode::Retire)).collect();
    apply_import_with(&pool, &fetcher, &path, &plan.file_hash, &modes, Some(3)).await.unwrap();
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM book_entry WHERE active")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "the three missing rows are retired, not purged");
}
