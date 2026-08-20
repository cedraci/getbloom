mod common;

use common::uniq;

#[tokio::test]
#[ignore = "requires postgres"]
async fn severity_quality_is_accepted_and_bogus_is_not() {
    let pool = common::pool().await;
    sqlx::query(
        "INSERT INTO ingest_issue (severity, code, detail)
         VALUES ('quality','quality_test','schema check')")
        .execute(&pool).await.expect("'quality' must pass the severity CHECK");
    let err = sqlx::query(
        "INSERT INTO ingest_issue (severity, code, detail)
         VALUES ('bogus','x','y')")
        .execute(&pool).await;
    assert!(err.is_err(), "unknown severities must still be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn field_def_qc_columns_default_to_disabled() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("QCCLS")).fetch_one(&pool).await.unwrap();
    let (nonpos, outlier, stale): (bool, Option<f64>, Option<i32>) = sqlx::query_as(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'t','numeric')
         RETURNING qc_nonpositive, qc_outlier_pct, qc_stale_days")
        .bind(class).bind(uniq("QCF")).fetch_one(&pool).await.unwrap();
    assert_eq!((nonpos, outlier, stale), (false, None, None),
               "every check is off unless the user turns it on");
    let bad = sqlx::query(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, qc_outlier_pct)
         VALUES ($1,$2,'t','numeric',-5)")
        .bind(class).bind(uniq("QCB")).execute(&pool).await;
    assert!(bad.is_err(), "a negative outlier threshold is meaningless");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_verify_dow_defaults_to_friday() {
    let pool = common::pool().await;
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("qsched")).fetch_one(&pool).await.unwrap();
    let (dow, last): (Option<i16>, Option<chrono::NaiveDate>) = sqlx::query_as(
        "INSERT INTO schedule (view_id) VALUES ($1) RETURNING verify_dow, last_verified_on")
        .bind(vid).fetch_one(&pool).await.unwrap();
    assert_eq!((dow, last), (Some(5), None));
    let bad = sqlx::query("UPDATE schedule SET verify_dow = 9 WHERE view_id = $1")
        .bind(vid).execute(&pool).await;
    assert!(bad.is_err(), "verify_dow is an ISO weekday, 1-7 or NULL");
}
