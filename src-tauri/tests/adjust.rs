mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::adjust::{self, AdjustMode};
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;
use getbloomdata_lib::instrument::store::{self, NewAlias};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// An instrument with a PX_LAST field, a run, and two RAW observations:
/// one before the split date, one after.
async fn scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let inst = store::create(pool).await.unwrap();
    let iid = inst.instrument_id;
    let sec = format!("{} US Equity", uniq(stem));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, iid, &NewAlias {
        id_type: "bdp_security".into(), value: sec.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(iid).bind(class).bind(&sec).execute(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric') RETURNING id")
        .bind(class).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("adjview")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();

    let cells = vec![
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2020-08-28"), value: CellValue::Num(400.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2020-09-01"), value: CellValue::Num(134.18) },
    ];
    ingest::ingest_outcome(pool, rid, &FetchOutcome { cells, problems: vec![] })
        .await.unwrap();
    (iid, fid)
}

async fn insert_factor(pool: &sqlx::PgPool, iid: i64, date: &str,
                       amount: f64, op: i16, flag: i16) {
    sqlx::query(
        "INSERT INTO corp_action
           (instrument_id, source_field, natural_key, event_date, amount,
            operator, flag, payload)
         VALUES ($1,'EQY_DVD_ADJUST_FACT',$2,$3,$4,$5,$6,'{}'::jsonb)")
        .bind(iid).bind(format!("{date}|{op}|{flag}")).bind(d(date))
        .bind(amount).bind(op).bind(flag)
        .execute(pool).await.unwrap();
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_series_adjusts_only_observations_before_the_event() {
    let pool = common::pool().await;
    let (iid, fid) = scaffold(&pool, "ADJONE").await;
    insert_factor(&pool, iid, "2020-08-31", 4.0, 1, 3).await;
    // An unparsed factor row (typed columns null, canonical-JSON key): must
    // be excluded and COUNTED, never silently half-applied.
    sqlx::query(
        "INSERT INTO corp_action (instrument_id, source_field, natural_key, payload)
         VALUES ($1,'EQY_DVD_ADJUST_FACT','{\"Mystery\":1}','{\"Mystery\":1}'::jsonb)")
        .bind(iid).execute(&pool).await.unwrap();

    let s = adjust::adjusted_series(&pool, iid, fid, AdjustMode::All, 500)
        .await.unwrap();
    assert_eq!(s.rows.len(), 2);
    assert_eq!((s.factors_used, s.unusable_factors), (1, 1));
    // obs_date DESC: [0] = post-split (untouched), [1] = pre-split (/4).
    assert_eq!((s.rows[0].raw, s.rows[0].adjusted), (134.18, 134.18));
    assert_eq!((s.rows[1].raw, s.rows[1].adjusted), (400.0, 100.0));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn raw_mode_returns_the_stored_values_untouched() {
    let pool = common::pool().await;
    let (iid, fid) = scaffold(&pool, "ADJTWO").await;
    insert_factor(&pool, iid, "2020-08-31", 4.0, 1, 3).await;
    let s = adjust::adjusted_series(&pool, iid, fid, AdjustMode::Raw, 500)
        .await.unwrap();
    assert_eq!(s.factors_used, 0);
    assert!(s.rows.iter().all(|r| r.raw == r.adjusted));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn splits_mode_ignores_dividend_factors() {
    let pool = common::pool().await;
    let (iid, fid) = scaffold(&pool, "ADJTHREE").await;
    insert_factor(&pool, iid, "2020-08-31", 4.0, 1, 3).await;   // split
    insert_factor(&pool, iid, "2020-08-30", 0.99, 2, 1).await;  // dividend
    let splits = adjust::adjusted_series(&pool, iid, fid, AdjustMode::Splits, 500)
        .await.unwrap();
    assert_eq!(splits.factors_used, 1, "only the flag-3 event");
    assert_eq!(splits.rows[1].adjusted, 100.0);
    let all = adjust::adjusted_series(&pool, iid, fid, AdjustMode::All, 500)
        .await.unwrap();
    assert_eq!(all.factors_used, 2);
    assert!((all.rows[1].adjusted - 400.0 * 0.99 / 4.0).abs() < 1e-9,
            "chronological: dividend (08-30) then split (08-31)");
}
