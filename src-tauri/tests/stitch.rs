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
async fn the_stitched_csv_carries_the_spliced_values() {
    let pool = common::pool().await;
    let (a, b, fid) = scaffold(&pool, "STCSV").await;
    link(&pool, a, b, "merger", "2026-01-12", true).await;
    let path = std::env::temp_dir().join(format!("{}.csv", uniq("stitch")));
    let n = stitch::export_stitched_csv(&pool, b, fid, AdjustMode::Raw, &path)
        .await.unwrap();
    assert_eq!(n, 4);
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "obs_date,value,source_instrument_id");
    assert_eq!(lines[3], format!("2026-01-05,25,{a}"));
    std::fs::remove_file(&path).ok();
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

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_roll_link_with_offset_is_accepted() {
    let pool = common::pool().await;
    let (a, _f, _) = scaffold(&pool, "RollA").await;
    let (b, _f2, _) = scaffold(&pool, "RollB").await;
    sqlx::query(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence, roll_offset)
         VALUES ($1, $2, 'roll', '2026-03-11', '{\"source\":\"test\"}', 2.5)")
        .bind(a).bind(b).execute(&pool).await
        .expect("'roll' must pass the link_type CHECK");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn roll_offset_is_rejected_on_a_merger() {
    let pool = common::pool().await;
    let (a, _f, _) = scaffold(&pool, "RollC").await;
    let (b, _f2, _) = scaffold(&pool, "RollD").await;
    let err = sqlx::query(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence, roll_offset)
         VALUES ($1, $2, 'merger', '2026-03-11', '{}', 2.5)")
        .bind(a).bind(b).execute(&pool).await;
    assert!(err.is_err(), "an additive offset is meaningless on a ratio link");
}

// ------------------------------------------ P9: the affine stitch composer

/// One class, one field with `mnemonic`, and an instrument per spec carrying
/// its own observations. The two-instrument `scaffold` above fixes both the
/// dates and the mnemonic; roll fixtures need three instruments, junction
/// dates of their own, and a VOLUME field.
async fn roll_scaffold(pool: &sqlx::PgPool, stem: &str, mnemonic: &str,
                       specs: &[(&str, &[(&str, f64)])]) -> (Vec<i64>, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'Field','numeric') RETURNING id")
        .bind(class).bind(mnemonic).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("rlview")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();

    let mut ids = Vec::new();
    for (label, cells) in specs {
        let inst = store::create(pool).await.unwrap();
        let iid = inst.instrument_id;
        let mut tx = pool.begin().await.unwrap();
        store::insert_alias(&mut tx, iid, &NewAlias {
            id_type: "bdp_security".into(),
            value: format!("{} US Equity", uniq(stem)),
            exch_code: Some("US".into()), valid_from: d("2000-01-03"),
            valid_to: None, source: "user".into(),
            bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                     VALUES ($1,$2,$3)")
            .bind(iid).bind(class).bind(*label).execute(pool).await.unwrap();
        let obs = cells.iter().map(|(obs_date, v)| ObsCell {
            instrument_id: iid, field_id: fid, obs_date: d(obs_date),
            value: CellValue::Num(*v),
        }).collect();
        ingest::ingest_outcome(pool, rid, &FetchOutcome { cells: obs, problems: vec![] })
            .await.unwrap();
        ids.push(iid);
    }
    (ids, fid)
}

/// A confirmed roll link. `propose_link` takes no offset, so the row goes in
/// directly; `roll_offset` NULL is the derive-it-from-the-junction case.
async fn roll_link(pool: &sqlx::PgPool, pred: i64, succ: i64, date: &str,
                   offset: Option<f64>) {
    sqlx::query(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence,
            roll_offset, confirmed_by, confirmed_at)
         VALUES ($1,$2,'roll',$3,'{\"test\":true}',$4,'test',now())")
        .bind(pred).bind(succ).bind(d(date)).bind(offset)
        .execute(pool).await.unwrap();
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_roll_link_splices_by_difference() {
    let pool = common::pool().await;
    // The 2026-03-09 row is what tells the two splices apart: AT the junction
    // observation a ratio and an offset agree by construction (98 * 100.5/98 =
    // 98 + 2.5), so only a value away from it can catch a multiplying composer.
    let (ids, fid) = roll_scaffold(&pool, "RollDiff", "PX_LAST", &[
        ("Front contract", &[("2026-03-09", 90.0), ("2026-03-10", 98.0)]),
        ("Back contract", &[("2026-03-11", 100.5), ("2026-03-12", 101.25)]),
    ]).await;
    let (b, c) = (ids[0], ids[1]);
    roll_link(&pool, b, c, "2026-03-11", Some(2.5)).await;

    let s = stitch::stitched_series(&pool, c, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    let b_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-10")).unwrap();
    assert!((b_row.value - 100.5).abs() < 1e-9,
            "a roll adds, it does not scale: {}", b_row.value);
    assert_eq!(b_row.source_instrument_id, b);
    let deep = s.rows.iter().find(|r| r.obs_date == d("2026-03-09")).unwrap();
    assert!((deep.value - 92.5).abs() < 1e-9,
            "90.0 + 2.5, not 90.0 * 100.5/98.0: {}", deep.value);
    let b_seg = s.segments.iter().find(|g| g.instrument_id == b).unwrap();
    assert_eq!(b_seg.offset, Some(2.5));
    assert_eq!(b_seg.ratio, None);
    assert_eq!(b_seg.link_type.as_deref(), Some("roll"));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_ratio_junction_scales_a_deeper_rolls_offset() {
    // Chain: A --merger(2.0 acquirer sh. per target sh.)--> B --roll(+3.0)--> C.
    // Walking back from C the roll comes first, then the merger, so the
    // merger must be dated strictly EARLIER (plan_chain descends).
    // A's 10.0 goes into B units at 10 * 0.5 = 5.0, then into C units at
    // 5.0 + 3.0 = 8.0 -- the merger's ratio must NOT rescale the roll offset
    // banked closer to the target: mul = 0.5, add = 3.0, 10*0.5 + 3 = 8.0.
    let pool = common::pool().await;
    // B's own junction value is 4.0, deliberately NOT the 5.0 that A maps to:
    // were they equal, scaling by the derived 8.0/5.0 and adding 3.0 would
    // give the same answer and the test would pin nothing.
    let (ids, fid) = roll_scaffold(&pool, "RollChain", "PX_LAST", &[
        ("Target", &[("2026-03-05", 10.0)]),
        ("Acquirer", &[("2026-03-09", 4.0)]),
        ("Rolled-into", &[("2026-03-11", 8.0)]),
    ]).await;
    let (a, b, c) = (ids[0], ids[1], ids[2]);
    let mid = store::propose_link(&pool, a, b, "merger", d("2026-03-06"),
                                  serde_json::json!({"test": true}))
        .await.unwrap();
    store::set_link_terms(&pool, mid, Some(2.0), None).await.unwrap();
    store::confirm_link(&pool, mid, "test").await.unwrap();
    roll_link(&pool, b, c, "2026-03-11", Some(3.0)).await;

    let s = stitch::stitched_series(&pool, c, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    let a_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-05")).unwrap();
    assert!((a_row.value - 8.0).abs() < 1e-9, "value: {}", a_row.value);
    let b_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-09")).unwrap();
    assert!((b_row.value - 7.0).abs() < 1e-9, "4.0 + 3.0: {}", b_row.value);
    let a_seg = s.segments.iter().find(|g| g.instrument_id == a).unwrap();
    assert_eq!(a_seg.ratio, Some(0.5));
    assert_eq!(a_seg.offset, None);
    let b_seg = s.segments.iter().find(|g| g.instrument_id == b).unwrap();
    assert_eq!(b_seg.offset, Some(3.0));
    assert_eq!(b_seg.ratio, None);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_roll_without_asserted_offset_derives_it_from_the_junction() {
    let pool = common::pool().await;
    let (ids, fid) = roll_scaffold(&pool, "RollDerive", "PX_LAST", &[
        ("Front contract", &[("2026-03-09", 90.0), ("2026-03-10", 98.0)]),
        ("Back contract", &[("2026-03-11", 101.0)]),
    ]).await;
    let (b, c) = (ids[0], ids[1]);
    roll_link(&pool, b, c, "2026-03-11", None).await;

    let s = stitch::stitched_series(&pool, c, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    let b_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-10")).unwrap();
    assert!((b_row.value - 101.0).abs() < 1e-9,
            "derived offset 101.0 - 98.0: {}", b_row.value);
    let deep = s.rows.iter().find(|r| r.obs_date == d("2026-03-09")).unwrap();
    assert!((deep.value - 93.0).abs() < 1e-9,
            "90.0 + 3.0, not 90.0 * 101.0/98.0: {}", deep.value);
    let b_seg = s.segments.iter().find(|g| g.instrument_id == b).unwrap();
    assert_eq!(b_seg.offset, Some(3.0));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn volumes_cross_a_roll_unscaled() {
    let pool = common::pool().await;
    let (ids, fid) = roll_scaffold(&pool, "RollVol", "PX_VOLUME", &[
        ("Front contract", &[("2026-03-10", 50_000.0)]),
        ("Back contract", &[("2026-03-11", 60_000.0)]),
    ]).await;
    let (b, c) = (ids[0], ids[1]);
    roll_link(&pool, b, c, "2026-03-11", Some(2.5)).await;

    let s = stitch::stitched_series(&pool, c, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    let b_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-10")).unwrap();
    assert!((b_row.value - 50_000.0).abs() < 1e-9,
            "a share count crosses a roll verbatim: {}", b_row.value);
}
