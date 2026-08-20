//! P6 merger-lifecycle flow against a real database and a mock fetcher
//! (design: docs/superpowers/specs/2026-08-20-p6-merger-lifecycle-design.md).

mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::adjust::AdjustMode;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::lifecycle;
use getbloomdata_lib::master_fetch::MockMasterFetcher;
use getbloomdata_lib::stitch;

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// The check date every test runs at: months after the last observation, so
/// the staleness trigger arms.
fn today() -> NaiveDate { d("2026-08-20") }

/// Two instruments in one class and one ACTIVE view, both with observations
/// that STOPPED in January -- stale by August. Returns
/// (pred_id, succ_id, field_id, pred_security, succ_security).
async fn scaffold(pool: &sqlx::PgPool, stem: &str)
    -> (i64, i64, i64, String, String)
{
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric') RETURNING id")
        .bind(class).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("lcview")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();

    let mut ids = Vec::new();
    let mut secs = Vec::new();
    for (label, cells) in [
        ("Dead target", vec![(d("2026-01-02"), 98.0), (d("2026-01-05"), 100.0)]),
        ("Live acquirer", vec![(d("2026-01-12"), 25.0), (d("2026-01-13"), 26.0)]),
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
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id)
                     VALUES ($1,$2)")
            .bind(vid).bind(iid).execute(pool).await.unwrap();
        let obs = cells.into_iter().map(|(obs_date, v)| ObsCell {
            instrument_id: iid, field_id: fid, obs_date,
            value: CellValue::Num(v),
        }).collect();
        ingest::ingest_outcome(pool, rid, &FetchOutcome { cells: obs, problems: vec![] })
            .await.unwrap();
        ids.push(iid);
        secs.push(sec);
    }
    (ids[0], ids[1], fid, secs[0].clone(), secs[1].clone())
}

/// Mock replies rebuilt from the 2026-08-20 live probe shapes, with the
/// test's own securities substituted for XLNX/AMD.
fn dead_equity_mock(pred_sec: &str, succ_sec: &str, action_id: &str)
    -> MockMasterFetcher
{
    let pred_ticker = pred_sec.strip_suffix(" Equity").unwrap();
    let succ_ticker = succ_sec.strip_suffix(" Equity").unwrap();
    let mut action_terms_raw = std::collections::HashMap::new();
    action_terms_raw.insert(action_id.to_string(), serde_json::json!([
        {"securityData": [{
            "security": format!("{action_id} Action"), "fieldExceptions": [],
            "fieldData": {
                "CA_MA_TARGET_TICKER": pred_ticker,
                "CA_MA_ACQUIRER_TICKER": succ_ticker,
                "CA_MA_ACQUIRER_NAME": "ACQUIRER INC",
                "CA_MA_COMPLETE_DT": "2026-01-12",
                "CA_MA_STOCK_TERMS": "2 Aqr sh./Tgt sh."}}]}]));
    MockMasterFetcher {
        market_status_raw: serde_json::json!([{"securityData": [
            {"security": pred_sec, "fieldExceptions": [],
             "fieldData": {"MARKET_STATUS": "ACQU"}}]}]),
        ma_deals_raw: serde_json::json!([
            {"security": pred_sec, "field": "MERGERS_AND_ACQUISITIONS", "rows": [
              {"Action Id": action_id, "Deal Type": "M&A",
               "Announcement Date": "2025-10-27", "Deal Status": "Completed"},
              {"Action Id": "111", "Deal Type": "INV",
               "Announcement Date": "2025-12-01", "Deal Status": "Completed"}]}]),
        action_terms_raw,
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_dead_equity_target_auto_links_with_bloombergs_terms() {
    let pool = common::pool().await;
    let (a, b, _fid, pred_sec, succ_sec) = scaffold(&pool, "LCEQ").await;
    let mock = dead_equity_mock(&pred_sec, &succ_sec, "900001");

    let s = lifecycle::run_check(&pool, &mock, today()).await.unwrap();
    // The acquirer is stale too, but the mock answers only for the target:
    // silence is "unknown", so only the target counts as checked.
    assert_eq!(s.checked, 1);
    assert_eq!(s.dead, 1);
    assert_eq!(s.links_proposed, 1);
    assert_eq!(s.links_confirmed, 1);

    let (confirmed_by, ratio, terms): (Option<String>, Option<f64>,
                                       Option<serde_json::Value>) =
        sqlx::query_as(
            "SELECT confirmed_by, exchange_ratio, terms FROM instrument_link
              WHERE predecessor_id = $1 AND successor_id = $2")
        .bind(a).bind(b).fetch_one(&pool).await.unwrap();
    assert_eq!(confirmed_by.as_deref(), Some("auto:action:900001"),
               "asserted by Bloomberg -> auto-confirmed");
    assert_eq!(ratio, Some(2.0));
    assert!(terms.unwrap().get("CA_MA_STOCK_TERMS").is_some(),
            "the verbatim terms payload rides on the link");

    let status: String = sqlx::query_scalar(
        "SELECT value FROM instrument_attr
          WHERE instrument_id = $1 AND attr = 'status'
            AND system_to = 'infinity'")
        .bind(a).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "ACQU", "the verbatim status lands in the reserved \
                                'status' attr slot");

    // Second pass, same day: the cooldown holds for the answered target;
    // the silent acquirer is legitimately asked again. Nothing duplicates.
    let s2 = lifecycle::run_check(&pool, &mock, today()).await.unwrap();
    assert_eq!(s2.checked, 0, "the recorded status arms the 30-day cooldown");
    assert_eq!(s2.links_proposed, 0);
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_link
          WHERE predecessor_id = $1 AND successor_id = $2")
        .bind(a).bind(b).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_stitched_series_prefers_the_asserted_ratio() {
    let pool = common::pool().await;
    let (a, b, fid, pred_sec, succ_sec) = scaffold(&pool, "LCST").await;
    let mock = dead_equity_mock(&pred_sec, &succ_sec, "900002");
    lifecycle::run_check(&pool, &mock, today()).await.unwrap();

    let s = stitch::stitched_series(&pool, b, fid, AdjustMode::Raw, 500)
        .await.unwrap();
    // Bloomberg said 2 acquirer shares per target share -> multiplier 1/2,
    // NOT the price-continuity 25/100 the derivation would have produced.
    assert_eq!(s.segments[1].ratio, Some(0.5));
    assert!(s.segments[1].note.as_deref().unwrap_or("").contains("Bloomberg"),
            "the segment says the splice came from asserted terms");
    assert!((s.rows[3].value - 98.0 * 0.5).abs() < 1e-9);
    assert_eq!(s.rows[3].source_instrument_id, a);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_dead_fund_gets_a_durable_issue_not_a_guessed_link() {
    let pool = common::pool().await;
    let (a, _b, _fid, pred_sec, _succ_sec) = scaffold(&pool, "LCFD").await;
    let mock = MockMasterFetcher {
        market_status_raw: serde_json::json!([{"securityData": [
            {"security": pred_sec, "fieldExceptions": [],
             "fieldData": {"MARKET_STATUS": "ACQU"}}]}]),
        // The fund answer, measured live: the M&A table is not applicable.
        ma_deals_problems: serde_json::json!([{
            "security": pred_sec, "field": "MERGERS_AND_ACQUISITIONS",
            "code": "field_error",
            "detail": "Field not applicable to security"}]),
        ..Default::default()
    };

    let s = lifecycle::run_check(&pool, &mock, today()).await.unwrap();
    assert_eq!(s.dead, 1);
    assert_eq!(s.links_confirmed, 0, "no successor is ever guessed for a fund");
    assert_eq!(s.issues, 1);

    let detail: String = sqlx::query_scalar(
        "SELECT detail FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'lifecycle_dead_fund'")
        .bind(a).fetch_one(&pool).await.unwrap();
    assert!(detail.contains("ACQU"), "the issue carries the verbatim status");
    assert!(detail.contains("CACX"), "and points at the manual path");

    let calls = mock.calls.lock().unwrap().clone();
    assert!(calls.iter().any(|c| c.starts_with("hist_ids:")),
            "identifier history was ingested to capture delist Action IDs");
}

/// An instrument that observed recently is not a candidate: the hook must
/// be free on ordinary days. Asserted on the candidate list itself, not on
/// run_check's call count -- the shared test database holds other tests'
/// deliberately-stale scaffolds in parallel.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_recently_observed_instrument_is_never_a_candidate() {
    let pool = common::pool().await;
    let (a, b, _fid, _ps, _ss) = scaffold(&pool, "LCOK").await;
    // 2026-01-14: the acquirer observed the day before (fresh); the target
    // stopped on the 5th (stale) -- a positive control that the query
    // separates the two rather than answering nothing.
    let cands = lifecycle::stale_candidates(&pool, d("2026-01-14")).await.unwrap();
    assert!(!cands.iter().any(|(id, _)| *id == b),
            "fresh observations keep an instrument out of the check entirely");
    assert!(cands.iter().any(|(id, _)| *id == a),
            "the quiet one is exactly what the check exists to ask about");
}
