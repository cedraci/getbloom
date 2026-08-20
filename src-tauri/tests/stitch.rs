mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::adjust::AdjustMode;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::stitch;

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// Two fund instruments in one class sharing a PX_LAST field, each with its
/// own observations. Returns (pred_id, succ_id, field_id).
async fn scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric') RETURNING id")
        .bind(class).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("stview")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();

    let mut ids = Vec::new();
    for (label, cells) in [
        ("Absorbed fund", vec![(d("2026-01-02"), 98.0), (d("2026-01-05"), 100.0)]),
        ("Surviving fund", vec![(d("2026-01-12"), 25.0), (d("2026-01-13"), 26.0)]),
    ] {
        let inst = store::create(pool).await.unwrap();
        let iid = inst.instrument_id;
        let sec = format!("{} US Equity", uniq(stem));
        let mut tx = pool.begin().await.unwrap();
        store::insert_alias(&mut tx, iid, &NewAlias {
            id_type: "bdp_security".into(), value: sec.clone(),
            exch_code: Some("US".into()), valid_from: d("2000-01-03"),
            valid_to: None, source: "user".into(),
            bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                     VALUES ($1,$2,$3)")
            .bind(iid).bind(class).bind(label).execute(pool).await.unwrap();
        let obs = cells.into_iter().map(|(obs_date, v)| ObsCell {
            instrument_id: iid, field_id: fid, obs_date,
            value: CellValue::Num(v),
        }).collect();
        ingest::ingest_outcome(pool, rid, &FetchOutcome { cells: obs, problems: vec![] })
            .await.unwrap();
        ids.push(iid);
    }
    (ids[0], ids[1], fid)
}

async fn link(pool: &sqlx::PgPool, pred: i64, succ: i64, ty: &str, date: &str,
              confirm: bool) {
    let id = store::propose_link(pool, pred, succ, ty, d(date),
                                 serde_json::json!({"test": true})).await.unwrap();
    if confirm {
        store::confirm_link(pool, id, "test").await.unwrap();
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_confirmed_merger_extends_the_survivor_backward() {
    let pool = common::pool().await;
    let (a, b, fid) = scaffold(&pool, "STONE").await;
    link(&pool, a, b, "merger", "2026-01-12", true).await;

    let s = stitch::stitched_series(&pool, b, fid, AdjustMode::Raw, 500)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    assert_eq!(s.rows.len(), 4);
    // DESC: B's two rows, then A's two spliced at ratio 25/100 = 0.25.
    assert_eq!((s.rows[0].obs_date, s.rows[0].value, s.rows[0].source_instrument_id),
               (d("2026-01-13"), 26.0, b));
    assert_eq!((s.rows[2].obs_date, s.rows[2].value, s.rows[2].source_instrument_id),
               (d("2026-01-05"), 25.0, a));
    assert!((s.rows[3].value - 98.0 * 0.25).abs() < 1e-9);
    assert_eq!(s.segments.len(), 2);
    assert_eq!(s.segments[1].ratio, Some(0.25));
    assert_eq!(s.segments[1].link_type.as_deref(), Some("merger"));
    assert_eq!(s.segments[1].label.as_deref(), Some("Absorbed fund"));
    assert!(stitch::has_confirmed_predecessors(&pool, b).await.unwrap());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_unconfirmed_link_is_never_followed() {
    let pool = common::pool().await;
    let (a, b, fid) = scaffold(&pool, "STTWO").await;
    link(&pool, a, b, "merger", "2026-01-12", false).await;
    let s = stitch::stitched_series(&pool, b, fid, AdjustMode::Raw, 500)
        .await.unwrap();
    assert_eq!(s.rows.len(), 2, "only the survivor's own rows");
    assert_eq!(s.segments.len(), 1);
    assert!(!stitch::has_confirmed_predecessors(&pool, b).await.unwrap());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_rename_splices_at_ratio_one() {
    let pool = common::pool().await;
    let (a, b, fid) = scaffold(&pool, "STTHREE").await;
    link(&pool, a, b, "rename", "2026-01-12", true).await;
    let s = stitch::stitched_series(&pool, b, fid, AdjustMode::Raw, 500)
        .await.unwrap();
    assert_eq!(s.rows[2].value, 100.0, "a rename is the same fund: no scaling");
    assert_eq!(s.segments[1].ratio, Some(1.0));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_missing_junction_observation_stops_the_walk_with_a_reason() {
    let pool = common::pool().await;
    let (a, b, fid) = scaffold(&pool, "STFOUR").await;
    // Wipe the predecessor's observations: no value before the junction.
    sqlx::query("DELETE FROM observation WHERE instrument_id = $1")
        .bind(a).execute(&pool).await.unwrap();
    link(&pool, a, b, "merger", "2026-01-12", true).await;
    let s = stitch::stitched_series(&pool, b, fid, AdjustMode::Raw, 500)
        .await.unwrap();
    assert_eq!(s.rows.len(), 2, "only the survivor's rows survive the stop");
    assert_eq!(s.segments.len(), 1);
    let reason = s.stopped.expect("a silent stop is a lie");
    assert!(reason.contains("ratio"), "reason: {reason}");
}
