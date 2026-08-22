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

#[tokio::test]
#[ignore = "requires postgres"]
async fn create_field_persists_qc_config() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("QCP")).fetch_one(&pool).await.unwrap();
    let f = getbloomdata_lib::fields::create_field(
        &pool, class, &uniq("px"), "Last", "numeric",
        None, None, "", true, Some(30.0), Some(5)).await.unwrap();
    assert!(f.qc_nonpositive);
    assert_eq!(f.qc_outlier_pct, Some(30.0));
    assert_eq!(f.qc_stale_days, Some(5));
    let err = getbloomdata_lib::fields::create_field(
        &pool, class, &uniq("nm"), "Name", "text",
        None, None, "", true, None, None).await;
    assert!(err.is_err(), "QC on a text field is a config mistake, said early");
}

use chrono::NaiveDate;
use getbloomdata_lib::fetch::{CellValue, FetchAsset, FetchField, FetchOutcome,
                              FetchRequest, ObsCell};
use getbloomdata_lib::{ingest, quality};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// Instrument + numeric field with QC on + view + run; returns ids.
async fn qc_scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                qc_nonpositive, qc_outlier_pct, qc_stale_days)
         VALUES ($1,$2,'Last','numeric',true,30,3) RETURNING id")
        .bind(class).bind(uniq("PXQ")).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("qgv")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','fetching') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (iid, fid, rid)
}

fn req_for(rid: i64, iid: i64, fid: i64, class: i64,
           start: NaiveDate, end: NaiveDate) -> FetchRequest {
    FetchRequest {
        run_id: rid,
        assets: vec![FetchAsset { instrument_id: iid, asset_class_id: class,
                                  class_name: "c".into(), label: "l".into(),
                                  bdp_security: "X US Equity".into() }],
        fields: vec![FetchField::daily_history(fid, class, "PX_LAST", "numeric")],
        start, end,
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_gate_writes_quality_issues_for_flagged_values() {
    let pool = common::pool().await;
    let (iid, fid, rid) = qc_scaffold(&pool, "QGATE").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM field_def WHERE id = $1")
        .bind(fid).fetch_one(&pool).await.unwrap();
    // Day 1 at 100, day 2 at 145 (outlier vs 30%), day 3 at -2 (nonpositive).
    let cells = vec![
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-11"), value: CellValue::Num(100.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-12"), value: CellValue::Num(145.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-13"), value: CellValue::Num(-2.0) },
    ];
    let outcome = FetchOutcome { cells, problems: vec![] };
    ingest::ingest_outcome(&pool, rid, &outcome).await.unwrap();
    let req = req_for(rid, iid, fid, class, d("2026-08-11"), d("2026-08-13"));
    let n = quality::run_quality_gate(&pool, rid, &req, &outcome).await.unwrap();
    assert!(n >= 2, "outlier + nonpositive at minimum, got {n}");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT severity, code FROM ingest_issue
          WHERE run_id = $1 AND severity = 'quality' ORDER BY code")
        .bind(rid).fetch_all(&pool).await.unwrap();
    let codes: Vec<&str> = rows.iter().map(|(_, c)| c.as_str()).collect();
    assert!(codes.contains(&"quality_outlier"), "codes: {codes:?}");
    assert!(codes.contains(&"quality_nonpositive"), "codes: {codes:?}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn unexplained_silence_becomes_quality_no_response() {
    let pool = common::pool().await;
    let (iid, fid, rid) = qc_scaffold(&pool, "QSIL").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM field_def WHERE id = $1")
        .bind(fid).fetch_one(&pool).await.unwrap();
    // Requested, but the outcome mentions it nowhere.
    let outcome = FetchOutcome { cells: vec![], problems: vec![] };
    let req = req_for(rid, iid, fid, class, d("2026-08-13"), d("2026-08-13"));
    let n = quality::run_quality_gate(&pool, rid, &req, &outcome).await.unwrap();
    assert_eq!(n, 1);
    let code: String = sqlx::query_scalar(
        "SELECT code FROM ingest_issue
          WHERE run_id = $1 AND instrument_id = $2 AND severity = 'quality'")
        .bind(rid).bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(code, "quality_no_response");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_superseded_value_leaves_a_visible_issue_and_unchanged_does_not() {
    let pool = common::pool().await;
    let (iid, fid, rid) = qc_scaffold(&pool, "QSUP").await;
    let cell = |v: f64| FetchOutcome {
        cells: vec![ObsCell { instrument_id: iid, field_id: fid,
                              obs_date: d("2026-08-13"),
                              value: CellValue::Num(v) }],
        problems: vec![],
    };
    ingest::ingest_outcome(&pool, rid, &cell(101.5)).await.unwrap();
    // Same value again: no supersession, no issue.
    let s2 = ingest::ingest_outcome(&pool, rid, &cell(101.5)).await.unwrap();
    assert_eq!((s2.superseded, s2.unchanged), (0, 1));
    // Restated value: superseded + a value_superseded issue naming both numbers.
    let s3 = ingest::ingest_outcome(&pool, rid, &cell(99.75)).await.unwrap();
    assert_eq!(s3.superseded, 1);
    let details: Vec<String> = sqlx::query_scalar(
        "SELECT detail FROM ingest_issue
          WHERE run_id = $1 AND code = 'value_superseded'")
        .bind(rid).fetch_all(&pool).await.unwrap();
    assert_eq!(details.len(), 1, "one alert for one restatement");
    assert!(details[0].contains("101.5") && details[0].contains("99.75"),
            "detail must name old and new: {}", details[0]);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_scheduled_verify_run_counts_as_todays_run() {
    let pool = common::pool().await;
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("vfyv")).fetch_one(&pool).await.unwrap();
    let today = chrono::Local::now().date_naive();
    sqlx::query(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'verify','scheduled','ok')")
        .bind(vid).execute(&pool).await.unwrap();
    assert!(getbloomdata_lib::scheduler::already_ran_today(&pool, vid, today)
        .await.unwrap(),
        "a completed scheduled verify must stop the EOD run from double-firing");
}

/// The run history is read by a person: 'backfill' for the weekly verification
/// pass reads as a manual catch-up, so verify runs carry their own kind.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_verify_run_is_recorded_as_kind_verify() {
    use getbloomdata_lib::error::AppResult;
    use getbloomdata_lib::fetch::FetchRequest;
    use getbloomdata_lib::orchestrator::{self, DataFetcher, PipelineConfig};
    use std::path::Path;

    struct EmptyFetcher;
    impl DataFetcher for EmptyFetcher {
        async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            Ok(FetchOutcome::default())
        }
    }

    let pool = common::pool().await;
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("vfyk")).fetch_one(&pool).await.unwrap();
    let cfg = PipelineConfig {
        data_dir: std::env::temp_dir(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit: 1_000_000,
        blp_host: None,
        blp_port: None,
    };
    orchestrator::run_verify_with(&pool, &cfg, &EmptyFetcher, vid,
                                  d("2026-08-17"), d("2026-08-21"))
        .await.unwrap();
    let (kind, trigger): (String, String) = sqlx::query_as(
        "SELECT kind, trigger_kind FROM run WHERE view_id = $1")
        .bind(vid).fetch_one(&pool).await.unwrap();
    assert_eq!((kind.as_str(), trigger.as_str()), ("verify", "scheduled"));
}
