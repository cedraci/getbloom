//! P9 capability flags: per-asset-class behaviour switches. See
//! docs/superpowers/specs/2026-08-21-p9-p10-multi-asset-and-production-ops-design.md.
mod common;

use common::uniq;
use getbloomdata_lib::registry;
use chrono::NaiveDate;
use getbloomdata_lib::adjust::{self, AdjustMode};
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;
use getbloomdata_lib::instrument::store::{self, NewAlias};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

#[tokio::test]
#[ignore = "requires postgres"]
async fn asset_class_capabilities_default_to_equity_shaped() {
    let pool = common::pool().await;
    let row: (bool, bool, String, Option<i32>) = sqlx::query_as(
        "INSERT INTO asset_class (name) VALUES ($1)
         RETURNING corp_actions_capable, ma_capable, adjustment_style, qc_stale_days_default")
        .bind(uniq("CapDflt")).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (true, true, "factors".to_string(), None));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn adjustment_style_rejects_unknown_values() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, adjustment_style) VALUES ($1, 'sideways')")
        .bind(uniq("CapBad")).execute(&pool).await;
    assert!(err.is_err(), "unknown adjustment styles must be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn stale_default_below_two_is_rejected() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, qc_stale_days_default) VALUES ($1, 1)")
        .bind(uniq("CapOne")).execute(&pool).await;
    assert!(err.is_err(), "a 1-day staleness window is meaningless (matches field_def CHECK)");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn capabilities_can_be_updated_and_read_back() {
    let pool = common::pool().await;
    let ac = registry::create_asset_class(&pool, &uniq("CapBond"), "").await.unwrap();
    registry::update_asset_class_capabilities(&pool, ac.id, false, false, "none", Some(8))
        .await.unwrap();
    let all = registry::list_asset_classes(&pool).await.unwrap();
    let got = all.iter().find(|c| c.id == ac.id).unwrap();
    assert!(!got.corp_actions_capable);
    assert!(!got.ma_capable);
    assert_eq!(got.adjustment_style, "none");
    assert_eq!(got.qc_stale_days_default, Some(8));
}

// ---------------------------------------------------------------------------
// corp_actions_capable gates both the pre-run estimate and the refresh seam
// ---------------------------------------------------------------------------

/// One instrument in a corp-actions-capable class, one in an incapable class,
/// same view. Returns (view_id, capable_iid, incapable_iid).
async fn two_class_view(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64) {
    let cap: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(&format!("{stem}Eq"))).fetch_one(pool).await.unwrap();
    let nocap: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, corp_actions_capable) VALUES ($1, FALSE) RETURNING id")
        .bind(uniq(&format!("{stem}Bond"))).fetch_one(pool).await.unwrap();
    let mut ids = Vec::new();
    for class in [cap, nocap] {
        let inst = getbloomdata_lib::instrument::store::create(pool).await.unwrap();
        sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label) VALUES ($1,$2,$3)")
            .bind(inst.instrument_id).bind(class).bind(uniq(stem))
            .execute(pool).await.unwrap();
        ids.push(inst.instrument_id);
    }
    let view: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq(&format!("{stem}V"))).fetch_one(pool).await.unwrap();
    for iid in &ids {
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
            .bind(view).bind(iid).execute(pool).await.unwrap();
    }
    (view, ids[0], ids[1])
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn corp_action_estimate_skips_incapable_classes() {
    let pool = common::pool().await;
    let (view, _cap, _nocap) = two_class_view(&pool, "CaEst").await;
    let est = getbloomdata_lib::orchestrator::corp_actions_estimate(&pool, view).await.unwrap();
    assert_eq!(est, 2, "1 capable member x 2 corp-action fields; the bond must not be counted");
}

/// Both members get a current `bdp_security` alias, so absent the capability
/// filter BOTH would be requested. Only the capable one may reach the wire --
/// and the excluded bond must not be counted as a no-security skip either
/// (Produces, task-3 brief): it never entered the batch at all.
#[tokio::test]
#[ignore = "requires postgres"]
async fn refresh_view_never_requests_an_incapable_member() {
    let pool = common::pool().await;
    let (view, cap, nocap) = two_class_view(&pool, "CaRef").await;
    let sec_cap = format!("{} US Equity", uniq("CaRefEq"));
    let sec_nocap = format!("{} US Corp", uniq("CaRefBond"));
    let mut tx = pool.begin().await.unwrap();
    for (iid, sec) in [(cap, &sec_cap), (nocap, &sec_nocap)] {
        getbloomdata_lib::instrument::store::insert_alias(&mut tx, iid,
            &getbloomdata_lib::instrument::store::NewAlias {
                id_type: "bdp_security".into(), value: sec.clone(),
                exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
                source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
            }).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mock = getbloomdata_lib::master_fetch::MockMasterFetcher::default();
    let summary = getbloomdata_lib::corp_actions::refresh_view(
        &pool, &mock, view, d("2026-08-21"), false).await.unwrap();

    let calls = mock.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), 1, "one batched call: {calls:?}");
    assert!(calls[0].contains(&sec_cap), "capable security must be requested: {calls:?}");
    assert!(!calls[0].contains(&sec_nocap),
            "the incapable member must never be requested: {calls:?}");
    assert_eq!(summary.instruments, 1);
    assert_eq!(summary.skipped, 0,
               "excluded by capability, not counted as a no-security skip");
}

// ---------------------------------------------------------------------------
// adjustment_style = 'none' short-circuits the factor engine
// ---------------------------------------------------------------------------

/// Like tests/adjust.rs::scaffold, but the class is `adjustment_style='none'`
/// and there is a single raw observation.
async fn none_style_scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, adjustment_style) VALUES ($1, 'none') RETURNING id")
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
        .bind(uniq("noneadjview")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();

    let cells = vec![
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-14"), value: CellValue::Num(100.0) },
    ];
    ingest::ingest_outcome(pool, rid, &FetchOutcome { cells, problems: vec![] })
        .await.unwrap();
    (iid, fid)
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_none_style_class_never_adjusts() {
    let pool = common::pool().await;
    let (iid, fid) = none_style_scaffold(&pool, "CapNone").await;
    // A flag-3 factor that WOULD halve raw history if the engine ran.
    sqlx::query(
        "INSERT INTO corp_action
           (instrument_id, source_field, natural_key, event_date, amount,
            operator, flag, payload)
         VALUES ($1,'EQY_DVD_ADJUST_FACT',$2,$3,$4,$5,$6,'{}'::jsonb)")
        .bind(iid).bind("2026-08-17|1|3").bind(d("2026-08-17"))
        .bind(0.5).bind(1i16).bind(3i16)
        .execute(&pool).await.unwrap();

    let s = adjust::adjusted_series(&pool, iid, fid, AdjustMode::All, 100).await.unwrap();
    assert_eq!(s.rows.len(), 1);
    assert_eq!(s.rows[0].raw, s.rows[0].adjusted, "'none' style must bypass the factor chain");
    assert_eq!(s.factors_used, 0);
    assert_eq!(s.unusable_factors, 0);
}

// ---------------------------------------------------------------------------
// qc_stale_days_default backstops the quality gate when field_def leaves the
// per-field threshold NULL; an explicit field-level value still wins.
// ---------------------------------------------------------------------------

use getbloomdata_lib::fetch::{FetchAsset, FetchField, FetchRequest};
use getbloomdata_lib::quality;

/// Instrument + numeric field (qc_stale_days = `field_stale`) in a class whose
/// `qc_stale_days_default` = `class_default`, plus a view + run. Returns ids.
async fn stale_scaffold(pool: &sqlx::PgPool, stem: &str,
                         field_stale: Option<i32>, class_default: Option<i32>)
                         -> (i64, i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, qc_stale_days_default) VALUES ($1,$2) RETURNING id")
        .bind(uniq(stem)).bind(class_default).fetch_one(pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, qc_stale_days)
         VALUES ($1,$2,'Last','numeric',$3) RETURNING id")
        .bind(class).bind(uniq("STF")).bind(field_stale).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("stalev")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','fetching') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (iid, fid, rid, class)
}

fn stale_req(rid: i64, iid: i64, fid: i64, class: i64,
             start: NaiveDate, end: NaiveDate) -> FetchRequest {
    FetchRequest {
        run_id: rid,
        assets: vec![FetchAsset { instrument_id: iid, asset_class_id: class,
                                  class_name: "c".into(), label: "l".into(),
                                  bdp_security: "X US Equity".into() }],
        fields: vec![FetchField { field_id: fid, asset_class_id: class,
                                  mnemonic: "PX_LAST".into(),
                                  value_kind: "numeric".into() }],
        start, end,
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn class_default_stale_window_fires_when_field_level_is_null() {
    let pool = common::pool().await;
    // field_def.qc_stale_days NULL, class default = 3.
    let (iid, fid, rid, class) =
        stale_scaffold(&pool, "StaleDflt", None, Some(3)).await;
    let cells = vec![
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-11"), value: CellValue::Num(7.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-12"), value: CellValue::Num(7.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-13"), value: CellValue::Num(7.0) },
    ];
    let outcome = FetchOutcome { cells, problems: vec![] };
    ingest::ingest_outcome(&pool, rid, &outcome).await.unwrap();
    let req = stale_req(rid, iid, fid, class, d("2026-08-11"), d("2026-08-13"));
    quality::run_quality_gate(&pool, rid, &req, &outcome).await.unwrap();
    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT code FROM ingest_issue
          WHERE run_id = $1 AND severity = 'quality' AND code = 'quality_stale'")
        .bind(rid).fetch_all(&pool).await.unwrap();
    assert!(!codes.is_empty(),
            "class default (3) must backstop a NULL field-level threshold");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn field_level_stale_days_wins_over_class_default() {
    let pool = common::pool().await;
    // field_def.qc_stale_days = 2, class default = 5 -- field wins, so two
    // identical values already fire (5 never would, over just two points).
    let (iid, fid, rid, class) =
        stale_scaffold(&pool, "StalePrec", Some(2), Some(5)).await;
    let cells = vec![
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-12"), value: CellValue::Num(9.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-13"), value: CellValue::Num(9.0) },
    ];
    let outcome = FetchOutcome { cells, problems: vec![] };
    ingest::ingest_outcome(&pool, rid, &outcome).await.unwrap();
    let req = stale_req(rid, iid, fid, class, d("2026-08-12"), d("2026-08-13"));
    quality::run_quality_gate(&pool, rid, &req, &outcome).await.unwrap();
    let codes: Vec<String> = sqlx::query_scalar(
        "SELECT code FROM ingest_issue
          WHERE run_id = $1 AND severity = 'quality' AND code = 'quality_stale'")
        .bind(rid).fetch_all(&pool).await.unwrap();
    assert!(!codes.is_empty(),
            "field-level threshold (2) must win over the class default (5)");
}
