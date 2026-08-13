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
