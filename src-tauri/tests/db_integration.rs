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
