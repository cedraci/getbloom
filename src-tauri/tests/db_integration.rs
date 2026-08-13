use sqlx::Row;

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
    let class = getbloomdata_lib::registry::create_asset_class(&pool, "EquityT3", "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(&pool, getbloomdata_lib::registry::NewAsset {
        asset_class_id: class.id,
        label: "Apple".into(),
        id_kind: "ticker".into(),
        ticker: Some("AAPL US".into()),
        isin: None,
        yellow_key: "Equity".into(),
    }).await.unwrap();
    assert_eq!(asset.bdp_security, "AAPL US Equity");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn view_fields_falls_back_to_class_fields() {
    use getbloomdata_lib::{db, fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, "EquityT4", "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "MC".into(), id_kind: "isin".into(),
        ticker: None, isin: Some("FR0000121014".into()), yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, "lux-t4", "").await.unwrap();
    views::set_view_assets(&pool, v.id, &[a.id]).await.unwrap();
    let fs = views::view_fields(&pool, v.id).await.unwrap();  // no explicit fields
    assert!(fs.iter().any(|x| x.id == f.id));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn ingest_twice_converges_no_duplicates() {
    use getbloomdata_lib::{db, fields, ingest, registry, views};
    use getbloomdata_lib::excel_read::{CellValue, ObsCell, ReadOutcome};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, "EquityT9", "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST_T9", "px", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "T9".into(), id_kind: "ticker".into(),
        ticker: Some("T9 US".into()), isin: None, yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, "t9-view", "").await.unwrap();
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ingesting') RETURNING id")
        .bind(v.id).fetch_one(&pool).await.unwrap();

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
    let mk = |val: f64| ReadOutcome {
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

#[tokio::test]
#[ignore = "requires postgres and excel"]
async fn eod_pipeline_dry_run_ends_partial() {
    use getbloomdata_lib::{db, fields, orchestrator, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, "EquityT11", "t").await.unwrap();
    fields::create_field(&pool, class.id, "PX_LAST_T11", "px", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "T11".into(), id_kind: "ticker".into(),
        ticker: Some("AAPL US".into()), isin: None, yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, "t11-view", "").await.unwrap();
    views::set_view_assets(&pool, v.id, &[a.id]).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = orchestrator::PipelineConfig {
        data_dir: dir.path().to_path_buf(),
        script_path: std::path::PathBuf::from("scripts/refresh.ps1"),
        refresh_timeout_s: 60,
        soft_limit: 100_000,
        dry_run_refresh: true,
    };
    let d = chrono::Local::now().date_naive();
    let out = orchestrator::run_eod(&pool, &cfg, v.id, "manual", d, false).await.unwrap();
    match out {
        orchestrator::RunOutcome::Completed { run_id, summary } => {
            assert!(summary.issues > 0);  // BDP can't evaluate without the add-in
            let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "partial");

            // Finding 3: assert the every-fetch-attempt ledger rule and the
            // pending/ -> archive/ workbook lifecycle actually happened end to end.
            let ledger_count: i64 = sqlx::query_scalar(
                "SELECT count(*)::bigint FROM hit_ledger WHERE run_id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(ledger_count, 1);

            let archive = orchestrator::archive_path(&cfg.data_dir, run_id, "t11-view", d);
            assert!(archive.exists(), "expected archived workbook at {archive:?}");

            let pending = orchestrator::pending_path(&cfg.data_dir, "t11-view", d);
            assert!(!pending.exists(), "pending workbook should have moved to archive/");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_draw_persists_within_day() {
    use getbloomdata_lib::{db, scheduler, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let v = views::create_view(&pool, "t12-view", "").await.unwrap();
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
