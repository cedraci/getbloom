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

// ---------------------------------------------------------------------------
// refresh_view: a whole set of stocks, batched
// ---------------------------------------------------------------------------

async fn view_with(pool: &sqlx::PgPool, instrument_ids: &[i64]) -> i64 {
    let vname = uniq("caview");
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(&vname).fetch_one(pool).await.unwrap();
    let class_name = uniq("CAClass");
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(&class_name).fetch_one(pool).await.unwrap();
    for iid in instrument_ids {
        sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                     VALUES ($1,$2,$3)")
            .bind(iid).bind(class).bind(format!("inst{iid}"))
            .execute(pool).await.unwrap();
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
            .bind(vid).bind(iid).execute(pool).await.unwrap();
    }
    vid
}

fn factor_table(sec: &str, date: &str, factor: f64) -> serde_json::Value {
    serde_json::json!({"security": sec, "field": "EQY_DVD_ADJUST_FACT",
        "rows": [{"Adjustment Date": date, "Adjustment Factor": factor,
                  "Adjustment Factor Operator Type": 1.0,
                  "Adjustment Factor Flag": 3.0}]})
}

/// Two members, ONE batched Bloomberg call, each instrument diffs its own
/// tables out of the shared response.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_view_refresh_covers_every_member_in_one_batched_call() {
    let pool = common::pool().await;
    let (a, sec_a) = instrument_with_security(&pool, "CAVA").await;
    let (b, sec_b) = instrument_with_security(&pool, "CAVB").await;
    let vid = view_with(&pool, &[a, b]).await;

    let mock = mock_with(serde_json::json!([
        factor_table(&sec_a, "2020-08-31", 4.0),
        factor_table(&sec_b, "2014-06-09", 7.0)]));
    let s = corp_actions::refresh_view(&pool, &mock, vid, d("2026-08-21")).await.unwrap();
    assert_eq!(s.instruments, 2);
    assert_eq!(s.inserted, 2);
    assert_eq!(s.skipped, 0);
    assert_eq!(mock.call_count(), 1, "two members, ONE batched call");
    for iid in [a, b] {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM corp_action WHERE instrument_id = $1")
            .bind(iid).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 1, "instrument {iid} got its own row");
    }
}

/// A member with no current security is skipped and reported -- and its dead
/// string never reaches the Bloomberg request.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_member_without_a_security_is_skipped_and_reported() {
    let pool = common::pool().await;
    let (a, sec_a) = instrument_with_security(&pool, "CAVC").await;
    let (b, _sec_b) = instrument_with_security(&pool, "CAVD").await;
    // Close b's only security in the past: nothing is valid today.
    sqlx::query("UPDATE instrument_alias SET valid_to = '2001-01-01'
                  WHERE instrument_id = $1 AND id_type = 'bdp_security'")
        .bind(b).execute(&pool).await.unwrap();
    let vid = view_with(&pool, &[a, b]).await;

    let mock = mock_with(serde_json::json!([factor_table(&sec_a, "2020-08-31", 4.0)]));
    let s = corp_actions::refresh_view(&pool, &mock, vid, d("2026-08-21")).await.unwrap();
    assert_eq!((s.instruments, s.skipped), (1, 1));
    let calls = mock.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert!(calls[0].contains(&sec_a) && !calls[0].contains("CAVD"),
            "the dead member must not be requested: {calls:?}");
    let issues: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'corp_actions_skipped'")
        .bind(b).fetch_one(&pool).await.unwrap();
    assert_eq!(issues, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_view_refresh_is_idempotent() {
    let pool = common::pool().await;
    let (a, sec_a) = instrument_with_security(&pool, "CAVE").await;
    let vid = view_with(&pool, &[a]).await;
    let mock = mock_with(serde_json::json!([factor_table(&sec_a, "2020-08-31", 4.0)]));
    corp_actions::refresh_view(&pool, &mock, vid, d("2026-08-21")).await.unwrap();
    let s2 = corp_actions::refresh_view(&pool, &mock, vid, d("2026-08-21")).await.unwrap();
    assert_eq!((s2.inserted, s2.unchanged), (0, 1), "second pass changes nothing");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action WHERE instrument_id = $1")
        .bind(a).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}
