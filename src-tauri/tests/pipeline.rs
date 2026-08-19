mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::error::AppResult;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, FetchRequest, ObsCell};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::orchestrator::{self, DataFetcher, PipelineConfig, RunOutcome};
use getbloomdata_lib::{ingest, views};
use std::path::Path;

fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

/// An instrument, a book entry, a view containing it, a field and a run.
async fn scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64, i64) {
    let class_name = uniq("PipeEquity");
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(&class_name)
        .fetch_one(pool).await.unwrap();
    let inst = store::create(pool).await.unwrap();
    let security = format!("{} US Equity", uniq(stem));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: security.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(inst.instrument_id).bind(class).bind(&security)
        .execute(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric')
         RETURNING id").bind(class).fetch_one(pool).await.unwrap();
    let vname = uniq("pipeview");
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(&vname).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(vid).bind(inst.instrument_id).execute(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (inst.instrument_id, fid, vid, rid)
}

fn outcome(instrument_id: i64, field_id: i64, run_date: &str, v: f64) -> FetchOutcome {
    FetchOutcome {
        cells: vec![ObsCell { instrument_id, field_id, obs_date: d(run_date),
                              value: CellValue::Num(v) }],
        problems: vec![],
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_ingested_observation_records_its_adjustment_basis() {
    let pool = common::pool().await;
    let (iid, fid, _, rid) = scaffold(&pool, "ZPIPE1").await;
    ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 100.0))
        .await.unwrap();
    let (layer, note): (String, String) = sqlx::query_as(
        "SELECT o.layer, b.note FROM observation o
           JOIN adjustment_basis b ON b.id = o.basis_id
          WHERE o.instrument_id = $1").bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(layer, "raw");
    assert!(note.starts_with("RAW"), "a price without its basis is not a fact");
}

/// The property the whole design exists to protect.
#[tokio::test]
#[ignore = "requires postgres"]
async fn re_ingesting_a_different_value_supersedes_rather_than_overwrites() {
    let pool = common::pool().await;
    let (iid, fid, _, rid) = scaffold(&pool, "ZPIPE2").await;
    ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 499.23))
        .await.unwrap();
    ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 124.81))
        .await.unwrap();

    let rows: Vec<(f64, bool)> = sqlx::query_as(
        "SELECT value_num, system_to = 'infinity' FROM observation
          WHERE instrument_id = $1 ORDER BY id").bind(iid)
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "the first value is retained, not replaced");
    assert_eq!(rows[0], (499.23, false), "superseded");
    assert_eq!(rows[1], (124.81, true), "current");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn re_ingesting_an_identical_value_changes_nothing() {
    let pool = common::pool().await;
    let (iid, fid, _, rid) = scaffold(&pool, "ZPIPE3").await;
    for _ in 0..3 {
        ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 100.0))
            .await.unwrap();
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM observation WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "an unchanged re-fetch must not grow the table every day");
}

/// Only a numeric price has an adjustment basis (schema
/// `observation_numeric_needs_basis`; the migration's comment that text-valued
/// fields "legitimately have none"). Asserting RAW for a text cell would be a
/// false claim about it, and would also let a text row and a future NULL-basis
/// writer both claim "current" for the same logical series without colliding
/// on `observation_current` -- so the lookup has to match NULL basis_id to
/// NULL, not just to the raw basis id.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_text_valued_observation_carries_no_adjustment_basis() {
    let pool = common::pool().await;
    let (iid, _, _, rid) = scaffold(&pool, "ZPIPE6").await;
    let class_id: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    // scaffold's own field (PX_LAST) is numeric; this test needs a text one.
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'NAME','Name','text') RETURNING id")
        .bind(class_id).fetch_one(&pool).await.unwrap();

    let text_outcome = |v: &str| FetchOutcome {
        cells: vec![ObsCell { instrument_id: iid, field_id: fid, obs_date: d("2026-08-18"),
                              value: CellValue::Text(v.into()) }],
        problems: vec![],
    };
    ingest::ingest_outcome(&pool, rid, &text_outcome("APPLE INC")).await.unwrap();
    ingest::ingest_outcome(&pool, rid, &text_outcome("APPLE INC.")).await.unwrap();

    let rows: Vec<(Option<String>, Option<i16>, bool)> = sqlx::query_as(
        "SELECT value_text, basis_id, system_to = 'infinity' FROM observation
          WHERE instrument_id = $1 AND field_id = $2 ORDER BY id")
        .bind(iid).bind(fid).fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "the first value is superseded, not duplicated");
    assert_eq!(rows[0].0.as_deref(), Some("APPLE INC"));
    assert_eq!(rows[0].1, None, "a text observation has no adjustment basis");
    assert!(!rows[0].2, "superseded");
    assert_eq!(rows[1].0.as_deref(), Some("APPLE INC."));
    assert_eq!(rows[1].1, None);
    assert!(rows[1].2, "current");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_view_lists_its_instruments_with_their_current_security_strings() {
    let pool = common::pool().await;
    let (iid, _, vid, _) = scaffold(&pool, "ZPIPE4").await;
    let members = views::view_instruments(&pool, vid).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].instrument_id, iid);
    assert!(members[0].security.as_deref().unwrap().starts_with("ZPIPE4"));
}

/// Spec §5: a pending review blocks the instrument from every view, so an
/// unresolved identifier cannot quietly become a gap in a time series.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_instrument_under_review_is_excluded_from_its_view() {
    let pool = common::pool().await;
    let (iid, _, vid, _) = scaffold(&pool, "ZPIPE5").await;
    let did: i64 = sqlx::query_scalar(
        "INSERT INTO resolution_decision
           (raw_input, normalized, method, chosen_instrument_id, candidates, decided_by)
         VALUES ('ZPIPE5','ZPIPE5 US Equity','manual',$1,'[]'::jsonb,'test')
         RETURNING id").bind(iid).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO resolution_review (decision_id, status)
                 VALUES ($1,'pending')").bind(did).execute(&pool).await.unwrap();

    assert!(views::view_instruments(&pool, vid).await.unwrap().is_empty());
}

struct EmptyFetcher;
impl DataFetcher for EmptyFetcher {
    async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>) -> AppResult<FetchOutcome> {
        Ok(FetchOutcome::default())
    }
}

/// A member with no security string valid today (here: its only alias's
/// `valid_to` is long past) must not vanish silently -- `eprintln!` reaches
/// nobody in a Tauri binary, so `load_view` also records it as an
/// `ingest_issue` with `run_id = NULL` (no run exists yet at that point in the
/// flow). A shrunk fetch that leaves no trace is indistinguishable from a
/// complete one.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_member_with_no_security_today_is_skipped_and_recorded_as_an_issue() {
    let pool = common::pool().await;
    let class_name = uniq("PipeEquitySkip");
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(&class_name).fetch_one(&pool).await.unwrap();
    let inst = store::create(&pool).await.unwrap();
    let security = format!("{} US Equity", uniq("ZPIPE7"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: security.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"),
        // Closed long before "today": current_security() finds nothing.
        valid_to: Some(d("2001-01-01")),
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(inst.instrument_id).bind(class).bind(&security)
        .execute(&pool).await.unwrap();
    let vname = uniq("pipeviewskip");
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(&vname).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(vid).bind(inst.instrument_id).execute(&pool).await.unwrap();

    let cfg = PipelineConfig {
        data_dir: std::env::temp_dir(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit: 1_000_000,
    };
    let outcome = orchestrator::run_eod_with(
        &pool, &cfg, &EmptyFetcher, vid, "manual", d("2026-08-18"), true)
        .await.unwrap();
    assert!(matches!(outcome, RunOutcome::Completed { .. }));

    let (code, detail): (String, String) = sqlx::query_as(
        "SELECT code, detail FROM ingest_issue
          WHERE instrument_id = $1 AND run_id IS NULL
          ORDER BY id DESC LIMIT 1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(code, "no_security_today");
    assert!(detail.contains(&inst.instrument_id.to_string()), "detail: {detail}");
}
