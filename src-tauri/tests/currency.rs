mod common;

use common::uniq;

#[tokio::test]
#[ignore = "requires postgres"]
async fn observation_currency_exists_and_is_append_only() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CCY")).fetch_one(&pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(&pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'Last','numeric') RETURNING id")
        .bind(class).bind(uniq("CPX")).fetch_one(&pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("ccyv")).fetch_one(&pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(&pool).await.unwrap();
    let basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false")
        .fetch_one(&pool).await.unwrap();
    let oid: i64 = sqlx::query_scalar(
        "INSERT INTO observation (instrument_id, field_id, obs_date, layer,
                                  basis_id, value_num, run_id, currency)
         VALUES ($1,$2,'2026-08-13','raw',$3,101.5,$4,'GBp') RETURNING id")
        .bind(iid).bind(fid).bind(basis).bind(rid)
        .fetch_one(&pool).await.unwrap();
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation WHERE id = $1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(stored.as_deref(), Some("GBp"), "verbatim, pence stay pence");
    let tampered = sqlx::query(
        "UPDATE observation SET currency = 'GBP' WHERE id = $1")
        .bind(oid).execute(&pool).await;
    assert!(tampered.is_err(),
            "currency is as immutable as the value it prices");
}

use chrono::NaiveDate;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

async fn ccy_scaffold(pool: &sqlx::PgPool, stem: &str, ccy: &str) -> (i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1,'currency',$2,'2000-01-01','user')")
        .bind(iid).bind(ccy).execute(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'Last','numeric') RETURNING id")
        .bind(class).bind(uniq("CPX")).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("ccyr")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (iid, fid, rid)
}

fn one_cell(iid: i64, fid: i64, v: f64) -> FetchOutcome {
    FetchOutcome {
        cells: vec![ObsCell { instrument_id: iid, field_id: fid,
                              obs_date: d("2026-08-13"),
                              value: CellValue::Num(v) }],
        problems: vec![],
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn ingest_stamps_the_instruments_currency_verbatim() {
    let pool = common::pool().await;
    let (iid, fid, rid) = ccy_scaffold(&pool, "CST", "GBp").await;
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, 4321.0)).await.unwrap();
    let ccy: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation
          WHERE instrument_id = $1 AND field_id = $2 AND system_to = 'infinity'")
        .bind(iid).bind(fid).fetch_one(&pool).await.unwrap();
    assert_eq!(ccy.as_deref(), Some("GBp"), "pence recorded as pence");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_currency_change_supersedes_and_raises_currency_changed() {
    let pool = common::pool().await;
    let (iid, fid, rid) = ccy_scaffold(&pool, "CCH", "EUR").await;
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, 100.0)).await.unwrap();
    // Redenomination: same value, the believed currency moves EUR -> USD.
    sqlx::query(
        "UPDATE instrument_attr SET system_to = now()
          WHERE instrument_id = $1 AND attr = 'currency' AND system_to = 'infinity'")
        .bind(iid).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1,'currency','USD','2000-01-01','user')")
        .bind(iid).execute(&pool).await.unwrap();
    let s = ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, 100.0)).await.unwrap();
    assert_eq!(s.superseded, 1, "same number, different unit: NOT unchanged");
    let code: String = sqlx::query_scalar(
        "SELECT code FROM ingest_issue
          WHERE run_id = $1 AND instrument_id = $2 AND code = 'currency_changed'")
        .bind(rid).bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(code, "currency_changed");
    let current: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation
          WHERE instrument_id = $1 AND field_id = $2 AND system_to = 'infinity'")
        .bind(iid).bind(fid).fetch_one(&pool).await.unwrap();
    assert_eq!(current.as_deref(), Some("USD"));
}

use getbloomdata_lib::adjust::AdjustMode;
use getbloomdata_lib::stitch;

/// Two instruments with one observation each and a confirmed merger link.
async fn linked_pair(pool: &sqlx::PgPool, stem: &str,
                     pred_ccy: &str, succ_ccy: &str) -> (i64, i64, i64) {
    let (pred, fid, rid) = ccy_scaffold(pool, stem, pred_ccy).await;
    let succ: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1,'currency',$2,'2000-01-01','user')")
        .bind(succ).bind(succ_ccy).execute(pool).await.unwrap();
    // predecessor priced before the junction, successor at/after it
    let basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false")
        .fetch_one(pool).await.unwrap();
    for (iid, date, px) in [(pred, "2026-06-30", 50.0), (succ, "2026-07-01", 100.0)] {
        sqlx::query(
            "INSERT INTO observation (instrument_id, field_id, obs_date, layer,
                                      basis_id, value_num, run_id)
             VALUES ($1,$2,$3::date,'raw',$4,$5,$6)")
            .bind(iid).bind(fid).bind(date).bind(basis).bind(px).bind(rid)
            .execute(pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO instrument_link (predecessor_id, successor_id, link_type,
                                      effective_date, evidence, exchange_ratio,
                                      confirmed_by, confirmed_at)
         VALUES ($1,$2,'merger','2026-07-01','{}'::jsonb,2.0,'test',now())")
        .bind(pred).bind(succ).execute(pool).await.unwrap();
    (pred, succ, fid)
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn stitching_refuses_a_cross_currency_junction() {
    let pool = common::pool().await;
    let (_pred, succ, fid) = linked_pair(&pool, "XCCY", "EUR", "USD").await;
    let s = stitch::stitched_series(&pool, succ, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.as_deref().unwrap_or("").contains("cross-currency"),
            "stopped: {:?}", s.stopped);
    assert!(s.rows.iter().all(|r| r.source_instrument_id == succ),
            "no predecessor rows may be spliced in a foreign currency");
}

/// P7 fix: `current_currency` used to require a period valid TODAY, but a
/// dead merger predecessor has ALL its attrs capped at the inactive date
/// (instrument/store.rs::close_attrs_at) -- so "valid today" finds nothing
/// for exactly the case the cross-currency guard exists for, and the guard
/// falls open. Cap the predecessor's currency period at the link date (as
/// death would) and confirm the guard still fires from the latest KNOWN
/// belief, not a "valid today" belief.
#[tokio::test]
#[ignore = "requires postgres"]
async fn stitching_refuses_a_cross_currency_junction_for_a_dead_predecessor() {
    let pool = common::pool().await;
    let (pred, succ, fid) = linked_pair(&pool, "DXCCY", "EUR", "USD").await;
    // Simulate the predecessor dying on the link's effective date: cap its
    // currency period there, exactly as close_attrs_at would.
    sqlx::query(
        "UPDATE instrument_attr SET valid_to = '2026-07-01'
          WHERE instrument_id = $1 AND attr = 'currency'")
        .bind(pred).execute(&pool).await.unwrap();
    let s = stitch::stitched_series(&pool, succ, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.as_deref().unwrap_or("").contains("cross-currency"),
            "a dead predecessor's currency belief must still gate the splice; \
             stopped: {:?}", s.stopped);
    assert!(s.rows.iter().all(|r| r.source_instrument_id == succ),
            "no predecessor rows may be spliced in a foreign currency");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn stitching_still_works_when_currencies_match() {
    let pool = common::pool().await;
    let (pred, succ, fid) = linked_pair(&pool, "SCCY", "USD", "USD").await;
    let s = stitch::stitched_series(&pool, succ, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    assert!(s.rows.iter().any(|r| r.source_instrument_id == pred),
            "the predecessor segment must be spliced");
}
