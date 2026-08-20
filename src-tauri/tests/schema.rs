//! Schema invariants. These are the constraints the design leans on; if one of
//! them stops holding, a later phase corrupts history silently rather than
//! failing loudly, so they are asserted directly against the database.

mod common;

use sqlx::{PgPool, Row};

/// Connects to the test database and runs migrations. Every integration test
/// starts here. Requires an EMPTY database on first run.
use common::pool as test_pool;

/// Creates a bare instrument and returns its id.
async fn new_instrument(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn migrations_apply_and_seed_the_two_adjustment_bases() {
    let pool = test_pool().await;
    let rows = sqlx::query("SELECT note, adj_normal, adj_abnormal, adj_split,
                                   adj_follow_dpdf FROM adjustment_basis ORDER BY id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "exactly RAW and LEGACY_DPDF are seeded");
    // RAW: all four flags explicitly false. P0 3.1 measured that this, and only
    // this, returns unadjusted prices.
    assert_eq!(rows[0].get::<Option<bool>, _>("adj_normal"), Some(false));
    assert_eq!(rows[0].get::<Option<bool>, _>("adj_split"), Some(false));
    // LEGACY_DPDF: unknown, because the flags were never set and the Terminal's
    // DPDF<GO> setting was not captured.
    assert_eq!(rows[1].get::<Option<bool>, _>("adj_normal"), None);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_alias_from_historical_ids_without_an_anchor_is_rejected() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let err = sqlx::query(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source)
         VALUES ($1, 'ticker', 'FB', DATE '2012-05-18', 'bloomberg_hist_ids')")
        .bind(iid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("alias_anchor_required"),
            "unanchored hist-ids alias must violate alias_anchor_required, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_alias_from_a_reference_request_needs_no_anchor() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    sqlx::query(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source)
         VALUES ($1, 'ticker', 'AAPL US', DATE '1980-12-12', 'bloomberg_ref')")
        .bind(iid).execute(&pool).await.expect("bloomberg_ref alias needs no anchor");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn id_bb_global_is_write_once() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let first = common::uniq("BBG");
    let second = common::uniq("BBG");
    sqlx::query("UPDATE instrument SET id_bb_global = $1 WHERE instrument_id = $2")
        .bind(&first).bind(iid).execute(&pool).await.expect("null -> value is allowed");
    let err = sqlx::query(
        "UPDATE instrument SET id_bb_global = $1 WHERE instrument_id = $2")
        .bind(&second).bind(iid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("write-once"),
            "overwriting a known FIGI must be refused, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn alias_provenance_columns_cannot_be_rewritten() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let aid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, exch_code, valid_from, source,
            bbg_action_id, anchoring_identifier)
         VALUES ($1, 'ticker', 'META', 'US', DATE '2022-06-09', 'bloomberg_hist_ids',
                 'ACT1', 'FB US Equity')
         RETURNING id")
        .bind(iid).fetch_one(&pool).await.unwrap();

    // Rewriting the anchor would launder an unanchored alias into a trusted
    // one -- the whole point of alias_anchor_required.
    let err = sqlx::query(
        "UPDATE instrument_alias SET anchoring_identifier = 'ROUNDHILL US Equity' WHERE id = $1")
        .bind(aid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("immutable"),
            "rewriting anchoring_identifier must be refused, got: {err}");

    let err = sqlx::query("UPDATE instrument_alias SET bbg_action_id = 'ACT2' WHERE id = $1")
        .bind(aid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("immutable"),
            "rewriting bbg_action_id must be refused, got: {err}");

    let err = sqlx::query("UPDATE instrument_alias SET exch_code = 'LN' WHERE id = $1")
        .bind(aid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("immutable"),
            "rewriting exch_code must be refused, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_alias_from_historical_ids_with_a_blank_anchor_is_rejected() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let err = sqlx::query(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source, anchoring_identifier)
         VALUES ($1, 'ticker', 'FB', DATE '2012-05-18', 'bloomberg_hist_ids', '   ')")
        .bind(iid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("alias_anchor_required"),
            "a blank anchor is not an anchor, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_alias_value_cannot_be_updated_but_can_be_closed() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let aid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source)
         VALUES ($1, 'ticker', 'FB', DATE '2012-05-18', 'user') RETURNING id")
        .bind(iid).fetch_one(&pool).await.unwrap();

    // Closing a validity period is the supported way to record a ticker change.
    sqlx::query("UPDATE instrument_alias SET valid_to = DATE '2022-06-09' WHERE id = $1")
        .bind(aid).execute(&pool).await.expect("closing valid_to is allowed");

    let err = sqlx::query("UPDATE instrument_alias SET value = 'META' WHERE id = $1")
        .bind(aid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("immutable"),
            "rewriting an alias value destroys history and must be refused, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_attr_source_cannot_be_rewritten() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let aid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1, 'name', 'Meta Platforms', DATE '2022-06-09', 'user') RETURNING id")
        .bind(iid).fetch_one(&pool).await.unwrap();
    let err = sqlx::query("UPDATE instrument_attr SET source = 'bloomberg' WHERE id = $1")
        .bind(aid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("immutable"),
            "rewriting source must be refused, matching instrument_alias's source freeze, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn only_one_current_row_per_logical_observation_series() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let basis = sqlx::query_scalar::<_, i16>(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false").fetch_one(&pool).await.unwrap();

    let insert = |v: f64| sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, granularity, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-18','eod','raw',$3,$4,$5)")
        .bind(iid).bind(fid).bind(basis).bind(v).bind(rid);

    insert(499.23).execute(&pool).await.expect("first current row");
    let err = insert(124.81).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("observation_current"),
            "a second CURRENT row for the same series must collide, got: {err}");

    // Superseding is legal: close the old row, then insert.
    sqlx::query("UPDATE observation SET system_to = now()
                 WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(iid).execute(&pool).await.unwrap();
    insert(124.81).execute(&pool).await.expect("a correction inserts beneath the closed row");
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM observation WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2, "the superseded row is retained, not replaced");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_eod_observation_must_have_no_time_and_an_intraday_one_must() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let basis = sqlx::query_scalar::<_, i16>(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false").fetch_one(&pool).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, obs_time, granularity, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-18',TIME '16:00','eod','raw',$3,1.0,$4)")
        .bind(iid).bind(fid).bind(basis).bind(rid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("observation_granularity_time"),
            "an EOD row carrying a time is ambiguous and must be refused, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_numeric_observation_without_a_basis_is_rejected() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let err = sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, granularity, layer, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-18','eod','raw',499.23,$3)")
        .bind(iid).bind(fid).bind(rid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("observation_numeric_needs_basis"),
            "a numeric price with no recorded adjustment basis is not a fact, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn observation_granularity_must_be_lowercase() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let basis = sqlx::query_scalar::<_, i16>(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false").fetch_one(&pool).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, obs_time, granularity, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-18',TIME '16:00','EOD','raw',$3,1.0,$4)")
        .bind(iid).bind(fid).bind(basis).bind(rid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("observation_granularity_lower"),
            "an uppercase granularity would silently fork observation_current's uniqueness key, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn observation_append_only_also_guards_time_granularity_and_run() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let basis = sqlx::query_scalar::<_, i16>(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false").fetch_one(&pool).await.unwrap();
    let oid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, granularity, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-19','eod','raw',$3,1.23,$4) RETURNING id")
        .bind(iid).bind(fid).bind(basis).bind(rid).fetch_one(&pool).await.unwrap();

    // Relocating a stored EOD row into the intraday series must be refused,
    // not silently accepted just because the destination shape is itself valid.
    let err = sqlx::query(
        "UPDATE observation SET granularity = 'intraday', obs_time = TIME '16:00' WHERE id = $1")
        .bind(oid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("append-only"),
            "changing granularity/obs_time must be refused, got: {err}");

    // Reassigning a stored observation to a different run must be refused too.
    let (_fid2, rid2) = seed_field_and_run(&pool, iid).await;
    let err = sqlx::query("UPDATE observation SET run_id = $1 WHERE id = $2")
        .bind(rid2).bind(oid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("append-only"),
            "changing run_id must be refused, got: {err}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn pg_trgm_is_available() {
    let pool = test_pool().await;
    let s: f32 = sqlx::query_scalar("SELECT similarity('AAPL US Equity', 'AAPL')")
        .fetch_one(&pool).await.expect("pg_trgm must be installed by the migration");
    assert!(s > 0.0);
}

/// A field_def and a run, so observation's foreign keys are satisfiable.
async fn seed_field_and_run(pool: &PgPool, _instrument_id: i64) -> (i64, i64) {
    let class_name = common::uniq("Equity");
    let cid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO asset_class (name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id")
        .bind(&class_name).fetch_one(pool).await.unwrap();
    let fid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric')
         ON CONFLICT (asset_class_id, mnemonic) DO UPDATE SET label = EXCLUDED.label
         RETURNING id").bind(cid).fetch_one(pool).await.unwrap();
    let view_name = common::uniq("v");
    let vid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(&view_name).fetch_one(pool).await.unwrap();
    let rid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (fid, rid)
}

/// Task 6 records evidence-based non-trading days; the PK is the dedup.
/// Task 7's auto-re-resolve cooldown needs to know WHEN an issue was written.
#[tokio::test]
#[ignore = "requires postgres"]
async fn non_trading_day_dedups_and_issues_are_timestamped() {
    let pool = common::pool().await;
    let inst = getbloomdata_lib::instrument::store::create(&pool).await.unwrap();
    let d: chrono::NaiveDate = "2026-08-14".parse().unwrap();
    sqlx::query("INSERT INTO non_trading_day (instrument_id, obs_date) VALUES ($1,$2)")
        .bind(inst.instrument_id).bind(d).execute(&pool).await.unwrap();
    let dup = sqlx::query("INSERT INTO non_trading_day (instrument_id, obs_date) VALUES ($1,$2)")
        .bind(inst.instrument_id).bind(d).execute(&pool).await;
    assert!(dup.is_err(), "the (instrument, date) PK must refuse a duplicate");

    sqlx::query("INSERT INTO ingest_issue (instrument_id, severity, code) VALUES ($1,'warn','x')")
        .bind(inst.instrument_id).execute(&pool).await.unwrap();
    let ts: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT created_at FROM ingest_issue WHERE instrument_id = $1 ORDER BY id DESC LIMIT 1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert!(ts <= chrono::Utc::now());
}

/// corp_action is snapshot-diffed: one current row per
/// (instrument, source_field, natural_key); amendments close-and-insert;
/// nothing may rewrite a payload in place.
#[tokio::test]
#[ignore = "requires postgres"]
async fn corp_action_is_append_only_with_one_current_row_per_key() {
    let pool = common::pool().await;
    let inst = getbloomdata_lib::instrument::store::create(&pool).await.unwrap();
    let ins = |payload: &'static str| {
        let pool = pool.clone();
        let iid = inst.instrument_id;
        async move {
            sqlx::query(
                "INSERT INTO corp_action
                   (instrument_id, source_field, natural_key, event_date, amount, payload)
                 VALUES ($1,'EQY_DVD_ADJUST_FACT','2020-08-31|1|3','2020-08-31',4.0,$2::jsonb)")
                .bind(iid).bind(payload).execute(&pool).await
        }
    };
    ins(r#"{"Adjustment Factor":4.0}"#).await.unwrap();
    let dup = ins(r#"{"Adjustment Factor":5.0}"#).await;
    assert!(dup.is_err(), "a second CURRENT row for the same key must be refused");

    let rewrite = sqlx::query(
        "UPDATE corp_action SET payload = '{}'::jsonb WHERE instrument_id = $1")
        .bind(inst.instrument_id).execute(&pool).await;
    assert!(rewrite.is_err(), "payload rewrite must be refused by the trigger");

    sqlx::query("UPDATE corp_action SET system_to = now() WHERE instrument_id = $1")
        .bind(inst.instrument_id).execute(&pool).await
        .expect("closing system_to is the one permitted update");
    ins(r#"{"Adjustment Factor":5.0}"#).await
        .expect("after closing, the corrected row inserts");
}
