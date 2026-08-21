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
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(vid).bind(fid).execute(pool).await.unwrap();
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
        blp_host: None,
        blp_port: None,
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

/// Corporate actions ride with every run (user decision 2026-08-21), so the
/// pre-run gate must price them in: +2 hits per member that will actually be
/// requested. A member flagged not-applicable is excluded from the estimate,
/// because it will not be requested.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_pre_run_gate_prices_in_corporate_actions() {
    let pool = common::pool().await;
    let (iid, _fid, vid, _rid) = scaffold(&pool, "CAGATE").await;
    let cfg = PipelineConfig {
        data_dir: std::env::temp_dir(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit: 0, // anything estimated > 0 forces the confirm gate
        blp_host: None,
        blp_port: None,
    };
    let out = orchestrator::run_eod_with(
        &pool, &cfg, &EmptyFetcher, vid, "manual", d("2026-08-18"), false)
        .await.unwrap();
    match out {
        RunOutcome::NeedsConfirmation { estimated, .. } =>
            assert_eq!(estimated, 1 + 2, "one price field + 2 corp-action hits"),
        other => panic!("expected NeedsConfirmation, got {other:?}"),
    }

    // A not-applicable member will not be requested, so it is not priced.
    sqlx::query("INSERT INTO corp_actions_na (instrument_id, detail)
                 VALUES ($1,'Field not applicable to security')")
        .bind(iid).execute(&pool).await.unwrap();
    let out = orchestrator::run_eod_with(
        &pool, &cfg, &EmptyFetcher, vid, "manual", d("2026-08-18"), false)
        .await.unwrap();
    match out {
        RunOutcome::NeedsConfirmation { estimated, .. } =>
            assert_eq!(estimated, 1, "the flagged member's corp actions cost nothing"),
        other => panic!("expected NeedsConfirmation, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// I2: gaps are per instrument, not per view.
// ---------------------------------------------------------------------------

/// `detect_gaps` collected `DISTINCT o.obs_date` across the whole view, so one
/// healthy instrument marked a date covered for every member. An instrument
/// that failed for a week, or was added mid-history, produced no gap and
/// therefore no backfill -- which is exactly the silent hole in a time series
/// the spec promises cannot happen, arrived at through the one function whose
/// job is to find them.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_gap_in_one_instrument_is_not_hidden_by_another_that_reported() {
    let pool = common::pool().await;
    let (healthy, fid, vid, rid) = scaffold(&pool, "GAPOK").await;

    // A second member of the same view, same class, that will report nothing.
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(healthy).fetch_one(&pool).await.unwrap();
    let silent_inst = store::create(&pool).await.unwrap();
    let silent = silent_inst.instrument_id;
    let silent_security = format!("{} US Equity", uniq("GAPBAD"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, silent, &NewAlias {
        id_type: "bdp_security".into(), value: silent_security.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(silent).bind(class).bind(&silent_security)
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(vid).bind(silent).execute(&pool).await.unwrap();

    // The healthy instrument reports on every weekday of the window; the
    // other reports on none of them.
    let today = d("2026-08-19");
    let mut day = today - chrono::Duration::days(6);
    while day < today {
        if !getbloomdata_lib::scheduler::is_weekend(day) {
            sqlx::query(
                "INSERT INTO observation
                   (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
                 VALUES ($1,$2,$3,'raw',1,1.0,$4)")
                .bind(healthy).bind(fid).bind(day).bind(rid)
                .execute(&pool).await.unwrap();
        }
        day += chrono::Duration::days(1);
    }

    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 6, today).await.unwrap();
    assert!(gaps.iter().all(|g| g.instrument_id != healthy),
            "the instrument that reported every day has no gap: {gaps:?}");
    assert!(gaps.iter().any(|g| g.instrument_id == silent),
            "the instrument that reported nothing MUST have one -- under the old \
             per-view query its silence was hidden by its neighbour: {gaps:?}");
    assert!(gaps.iter().filter(|g| g.instrument_id == silent)
                .any(|g| g.label == silent_security),
            "a gap names the instrument, so the user can see which one is missing");
}

/// The members come from `views::view_instruments`, so an instrument retired
/// from the book is not reported as a gap: it is not supposed to be
/// collecting, and a permanent, unfixable "gap" is noise that hides the real
/// ones.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_retired_member_is_not_reported_as_a_gap() {
    let pool = common::pool().await;
    let (inst, _fid, vid, _rid) = scaffold(&pool, "GAPRET").await;
    let today = d("2026-08-19");

    let before = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 6, today).await.unwrap();
    assert!(before.iter().any(|g| g.instrument_id == inst),
            "an active member with no observations is a gap");

    sqlx::query("UPDATE book_entry SET active = FALSE WHERE instrument_id = $1")
        .bind(inst).execute(&pool).await.unwrap();
    let after = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 6, today).await.unwrap();
    assert!(after.iter().all(|g| g.instrument_id != inst),
            "a retired member is not collecting, so it is not a gap: {after:?}");
}

/// Gap detection is per (instrument, field-complete date): a PX_LAST hole
/// must not hide behind a PX_VOLUME that succeeded. Text fields do not
/// count -- backfill cannot recover them by design, and an unfixable gap is
/// noise that buries the fixable ones.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_missing_field_behind_a_present_sibling_is_still_a_gap() {
    let pool = common::pool().await;
    let (iid, px_last, vid, rid) = scaffold(&pool, "GAPFLD").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    let px_volume: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_VOLUME','Volume','numeric') RETURNING id")
        .bind(class).fetch_one(&pool).await.unwrap();
    let name_fld: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'NAME','Name','text') RETURNING id")
        .bind(class).fetch_one(&pool).await.unwrap();
    for f in [px_volume, name_fld] {
        sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
            .bind(vid).bind(f).execute(&pool).await.unwrap();
    }

    // Tuesday 2026-08-18: PX_LAST present, PX_VOLUME missing, NAME missing.
    let day = d("2026-08-18");
    sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,$3,'raw',1,100.0,$4)")
        .bind(iid).bind(px_last).bind(day).bind(rid)
        .execute(&pool).await.unwrap();

    let today = d("2026-08-19");
    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 1, today).await.unwrap();
    assert!(gaps.iter().any(|g| g.instrument_id == iid && g.start == day),
            "PX_VOLUME missing on {day} must surface as a gap: {gaps:?}");

    // Fill PX_VOLUME; NAME (text) stays absent -- and must not keep the gap open.
    sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,$3,'raw',1,5.0e6,$4)")
        .bind(iid).bind(px_volume).bind(day).bind(rid)
        .execute(&pool).await.unwrap();
    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 1, today).await.unwrap();
    assert!(gaps.iter().all(|g| g.instrument_id != iid),
            "both numeric fields present; a missing TEXT field is not a gap: {gaps:?}");
}

/// A one-instrument gap must not cost a whole-view refetch. The filter is
/// applied at load_view, so the estimate, the request and the ingest all see
/// only the targeted instrument.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_filtered_backfill_fetches_only_the_target_instrument() {
    struct Recording(std::sync::Mutex<Vec<Vec<i64>>>);
    impl DataFetcher for Recording {
        async fn fetch(&self, req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            self.0.lock().unwrap()
                .push(req.assets.iter().map(|a| a.instrument_id).collect());
            Ok(FetchOutcome::default())
        }
    }

    let pool = common::pool().await;
    let (a, _fid, vid, _rid) = scaffold(&pool, "BFONE").await;
    // Second member, same class.
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(a).fetch_one(&pool).await.unwrap();
    let b_inst = store::create(&pool).await.unwrap();
    let b = b_inst.instrument_id;
    let b_sec = format!("{} US Equity", uniq("BFTWO"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, b, &NewAlias {
        id_type: "bdp_security".into(), value: b_sec.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(b).bind(class).bind(&b_sec).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(vid).bind(b).execute(&pool).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = PipelineConfig {
        data_dir: dir.path().to_path_buf(),
        python_path: "python".into(), script_path: "scripts/blp_fetch.py".into(),
        request_timeout_s: 5, soft_limit: 100_000,
        blp_host: None, blp_port: None,
    };
    let rec = Recording(std::sync::Mutex::new(Vec::new()));
    let out = orchestrator::run_backfill_with(
        &pool, &cfg, &rec, vid, d("2026-08-17"), d("2026-08-18"),
        Some(&[b]), true).await.unwrap();
    assert!(matches!(out, RunOutcome::Completed { .. }));
    let seen = rec.0.lock().unwrap().clone();
    assert_eq!(seen, vec![vec![b]],
               "only the targeted instrument may reach the fetcher: {seen:?}");
}

/// Rule A: a dated no_data with no cells and no other problem for that
/// (instrument, day) is Bloomberg saying "no trading session". Recording it
/// is what lets detect_gaps stop reporting the holiday forever.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_dated_no_data_marks_a_non_trading_day_and_clears_the_gap() {
    let pool = common::pool().await;
    let (iid, fid, vid, rid) = scaffold(&pool, "NTD1").await;
    let holiday = d("2026-08-18");
    let req = FetchRequest { run_id: rid, assets: vec![], fields: vec![],
                             start: holiday, end: holiday };
    let out = FetchOutcome {
        cells: vec![],
        problems: vec![getbloomdata_lib::fetch::CellProblem {
            instrument_id: Some(iid), field_id: Some(fid),
            obs_date: Some(holiday), code: "no_data".into(),
            detail: "no trading day returned".into(),
        }],
    };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 1);

    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 1, d("2026-08-19"))
        .await.unwrap();
    assert!(gaps.iter().all(|g| g.instrument_id != iid),
            "a recorded non-trading day is not a gap: {gaps:?}");
}

/// An invalid_security day is NOT a holiday -- nothing may be recorded.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_invalid_security_day_is_not_recorded_as_non_trading() {
    let pool = common::pool().await;
    let (iid, fid, _vid, rid) = scaffold(&pool, "NTD2").await;
    let day = d("2026-08-18");
    let req = FetchRequest { run_id: rid, assets: vec![], fields: vec![],
                             start: day, end: day };
    let out = FetchOutcome {
        cells: vec![],
        problems: vec![
            getbloomdata_lib::fetch::CellProblem {
                instrument_id: Some(iid), field_id: Some(fid),
                obs_date: Some(day), code: "no_data".into(), detail: String::new() },
            getbloomdata_lib::fetch::CellProblem {
                instrument_id: Some(iid), field_id: None,
                obs_date: Some(day), code: "invalid_security".into(), detail: String::new() },
        ],
    };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 0, "mixed signals are not holiday evidence");
}

/// Rule B: inside a multi-day range, ACTIVE_DAYS_ONLY omits a non-trading
/// day silently. A weekday with no cells, for an instrument that DID return
/// cells on other days of the range, is non-trading by inference.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_silent_weekday_inside_a_range_with_neighbours_is_non_trading() {
    let pool = common::pool().await;
    let (iid, fid, _vid, rid) = scaffold(&pool, "NTD3").await;
    let (mon, tue, wed) = (d("2026-08-17"), d("2026-08-18"), d("2026-08-19"));
    let req = FetchRequest { run_id: rid, assets: vec![], fields: vec![],
                             start: mon, end: wed };
    let cell = |dt| ObsCell { instrument_id: iid, field_id: fid, obs_date: dt,
                              value: CellValue::Num(1.0) };
    let out = FetchOutcome { cells: vec![cell(mon), cell(wed)], problems: vec![] };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 1);
    let src: String = sqlx::query_scalar(
        "SELECT source FROM non_trading_day WHERE instrument_id=$1 AND obs_date=$2")
        .bind(iid).bind(tue).fetch_one(&pool).await.unwrap();
    assert_eq!(src, "range_inference");
}

/// A BulkFormat field configured on a view must be SKIPPED by the EOD
/// pipeline (with an ingest_issue naming it), not stringified -- the exact
/// failure the sidecar's parse_bulk_message docstring warns about.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_bulk_field_on_a_view_is_skipped_with_an_issue_not_stringified() {
    let pool = common::pool().await;
    let (iid, _fid, vid, _rid) = scaffold(&pool, "BULKG").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    let bulk_fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, bbg_ftype)
         VALUES ($1,'DVD_HIST_ALL_WITH_AMT_STATUS','Dividends','text','BulkFormat')
         RETURNING id").bind(class).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(vid).bind(bulk_fid).execute(&pool).await.unwrap();

    struct Silent;
    impl DataFetcher for Silent {
        async fn fetch(&self, req: &FetchRequest, _a: Option<&Path>)
            -> AppResult<FetchOutcome> {
            assert!(req.fields.iter().all(|f| f.mnemonic != "DVD_HIST_ALL_WITH_AMT_STATUS"),
                    "a bulk field must never reach the fetcher");
            Ok(FetchOutcome::default())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = PipelineConfig { data_dir: dir.path().to_path_buf(),
        python_path: "python".into(), script_path: "scripts/blp_fetch.py".into(),
        request_timeout_s: 5, soft_limit: 100_000,
        blp_host: None, blp_port: None };
    orchestrator::run_eod_with(&pool, &cfg, &Silent, vid, "manual",
                               d("2026-08-18"), true).await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE code = 'bulk_field_skipped' AND detail LIKE '%DVD_HIST_ALL%'")
        .fetch_one(&pool).await.unwrap();
    assert!(n >= 1, "the skip must be visible, not silent");
}
