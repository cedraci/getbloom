mod common;

use common::uniq;
use getbloomdata_lib::corp_actions;
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::master_fetch::MockMasterFetcher;

fn d(s: &str) -> chrono::NaiveDate { s.parse().unwrap() }

async fn instrument_with_security(pool: &sqlx::PgPool, stem: &str) -> (i64, String) {
    let inst = store::create(pool).await.unwrap();
    let sec = format!("{} US Equity", uniq(stem));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: sec.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    (inst.instrument_id, sec)
}

fn mock_with(rows: serde_json::Value) -> MockMasterFetcher {
    MockMasterFetcher { corp_actions_raw: rows, ..Default::default() }
}

fn factor_rows(sec: &str, factor: f64) -> serde_json::Value {
    serde_json::json!([{"security": sec, "field": "EQY_DVD_ADJUST_FACT",
        "rows": [{"Adjustment Date": "2020-08-31", "Adjustment Factor": factor,
                  "Adjustment Factor Operator Type": 1.0,
                  "Adjustment Factor Flag": 3.0}]}])
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_refresh_inserts_and_a_second_identical_refresh_converges() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CAONE").await;
    let mock = mock_with(factor_rows(&sec, 4.0));
    let s1 = corp_actions::refresh(&pool, &mock, iid, d("2026-08-20")).await.unwrap();
    assert_eq!((s1.inserted, s1.unchanged), (1, 0));
    let s2 = corp_actions::refresh(&pool, &mock, iid, d("2026-08-20")).await.unwrap();
    assert_eq!((s2.inserted, s2.unchanged), (0, 1), "identical snapshot inserts nothing");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_amended_amount_supersedes_and_keeps_the_old_belief() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CATWO").await;
    corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 4.0)), iid,
                          d("2026-08-20")).await.unwrap();
    let s = corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 5.0)), iid,
                                  d("2026-08-20")).await.unwrap();
    assert_eq!(s.amended, 1);
    let rows: Vec<(f64, bool)> = sqlx::query_as(
        "SELECT amount, system_to = 'infinity' FROM corp_action
          WHERE instrument_id = $1 ORDER BY id")
        .bind(iid).fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![(4.0, false), (5.0, true)],
               "the old belief is closed, never destroyed");
}

/// A key that vanishes from a NON-EMPTY fresh snapshot is a withdrawn
/// action: closed, and reported as an ingest_issue.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_vanished_action_is_closed_and_reported() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CATHREE").await;
    let two = serde_json::json!([{"security": sec, "field": "EQY_DVD_ADJUST_FACT",
        "rows": [
          {"Adjustment Date": "2020-08-31", "Adjustment Factor": 4.0,
           "Adjustment Factor Operator Type": 1.0, "Adjustment Factor Flag": 3.0},
          {"Adjustment Date": "2014-06-09", "Adjustment Factor": 7.0,
           "Adjustment Factor Operator Type": 1.0, "Adjustment Factor Flag": 3.0}]}]);
    corp_actions::refresh(&pool, &mock_with(two), iid, d("2026-08-20")).await.unwrap();
    let s = corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 4.0)), iid,
                                  d("2026-08-20")).await.unwrap();
    assert_eq!(s.withdrawn, 1);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action
          WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(open, 1);
    let issues: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'corp_action_withdrawn'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(issues, 1);
}

/// An EMPTY table for a source_field must close nothing: a whole-history
/// cancellation is not a real scenario, but a failed field in the response
/// producing zero rows is -- and it must not wipe the local history.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_empty_snapshot_for_a_field_closes_nothing() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CAFOUR").await;
    corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 4.0)), iid,
                          d("2026-08-20")).await.unwrap();
    let s = corp_actions::refresh(&pool, &mock_with(serde_json::json!([])), iid,
                                  d("2026-08-20")).await.unwrap();
    assert_eq!(s.withdrawn, 0);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action
          WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(open, 1, "an absent table is a failed fetch, not a cancellation");
}

/// Rows the tolerant parser could not type still land (payload verbatim,
/// canonical-JSON key) and are counted for the summary + ingest_issue.
#[tokio::test]
#[ignore = "requires postgres"]
async fn unparsed_rows_are_stored_flagged_and_counted() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CAFIVE").await;
    let odd = serde_json::json!([{"security": sec,
        "field": "DVD_HIST_ALL_WITH_AMT_STATUS",
        "rows": [{"Unexpected Shape": 1}]}]);
    let s = corp_actions::refresh(&pool, &mock_with(odd), iid, d("2026-08-20"))
        .await.unwrap();
    assert_eq!((s.inserted, s.unparsed), (1, 1));
    let issues: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'corp_action_unparsed'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(issues, 1);
}
