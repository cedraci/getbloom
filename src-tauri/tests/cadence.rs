//! P11 cadence + fetch-capability + identity-sweep schema (migration 0014).
//! See docs/superpowers/specs/2026-08-22-p11-cadence-and-fetch-capability-design.md
//! sections 11.1, 11.2, 11.8.
mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::error::AppResult;
use getbloomdata_lib::fetch::{
    CellProblem, CellValue, FetchAsset, FetchField, FetchOutcome, FetchRequest, ObsCell,
    PeriodicLeg, RequestSpec,
};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::master_fetch::{
    ActionTerms, Answered, CorpActionsTables, HistIdRow, IdentityBlock, MaDealsOutcome,
    MasterFetcher, MockMasterFetcher, SweepAnswer,
};
use getbloomdata_lib::orchestrator::{self, DataFetcher, PipelineConfig, RunOutcome};
use getbloomdata_lib::resolution::score::Candidate;
use getbloomdata_lib::{
    fetch, fields, identity, ingest, master_fetch, quality, registry, scheduler, views,
};
use std::path::Path;

// ---------------------------------------------------------------------------
// 11.1 Effective cadence = COALESCE(field_def.cadence, asset_class.default_cadence)
// -- the same COALESCE idiom quality.rs uses for qc_stale_days.
// ---------------------------------------------------------------------------

/// Resolve a field's effective cadence *through production*: `views::view_fields`
/// is the planner's field-metadata source, and its `effective_cadence` column
/// is the only COALESCE the pipeline actually consults. Asserting against a
/// second, test-local copy of the SQL would pin the copy, not the behaviour.
async fn effective_cadence(pool: &sqlx::PgPool, field_id: i64) -> String {
    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadEffView")).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(view).bind(field_id).execute(pool).await.unwrap();
    let fields = views::view_fields(pool, view).await.unwrap();
    fields.iter().find(|f| f.def.id == field_id)
        .expect("the view's own field must come back from view_fields")
        .effective_cadence.clone()
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn effective_cadence_prefers_field_override_over_class_default() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, default_cadence) VALUES ($1, 'monthly') RETURNING id")
        .bind(uniq("CadClass")).fetch_one(&pool).await.unwrap();
    let field: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, cadence)
         VALUES ($1, $2, 'Last', 'numeric', 'daily') RETURNING id")
        .bind(class).bind(uniq("CADF")).fetch_one(&pool).await.unwrap();

    assert_eq!(effective_cadence(&pool, field).await, "daily",
        "an explicit field-level cadence override must win over the class default");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn effective_cadence_falls_back_to_class_default_when_field_cadence_is_null() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, default_cadence) VALUES ($1, 'monthly') RETURNING id")
        .bind(uniq("CadClass2")).fetch_one(&pool).await.unwrap();
    let field: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1, $2, 'NAV', 'numeric') RETURNING id")
        .bind(class).bind(uniq("CADF2")).fetch_one(&pool).await.unwrap();

    assert_eq!(effective_cadence(&pool, field).await, "monthly",
        "a NULL field-level cadence must defer to the class default");
}

/// `view_fields` has TWO query paths -- the explicit `view_field` rows above,
/// and this one: the spec default of "every active field of the classes the
/// view's instruments belong to". The planner reads whichever fires, so the
/// COALESCE has to live in both.
#[tokio::test]
#[ignore = "requires postgres"]
async fn view_fields_resolves_cadence_and_fetch_via_on_the_class_default_branch() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, default_cadence) VALUES ($1, 'quarterly') RETURNING id")
        .bind(uniq("CadDfltBranch")).fetch_one(&pool).await.unwrap();
    let field: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, fetch_via)
         VALUES ($1, $2, 'Yield', 'numeric', 'reference') RETURNING id")
        .bind(class).bind(uniq("CADFB2")).fetch_one(&pool).await.unwrap();
    let instrument: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label) VALUES ($1,$2,$3)")
        .bind(instrument).bind(class).bind(uniq("CadBook")).execute(&pool).await.unwrap();
    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadDfltView")).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(view).bind(instrument).execute(&pool).await.unwrap();

    let got = views::view_fields(&pool, view).await.unwrap();
    let f = got.iter().find(|f| f.def.id == field)
        .expect("the class-default branch must return the class's active fields");
    assert_eq!(f.effective_cadence, "quarterly",
        "the default branch resolves the class cadence too, not just the explicit one");
    assert_eq!(f.def.fetch_via, "reference",
        "fetch_via rides along on the field row the planner partitions by");
}

// ---------------------------------------------------------------------------
// Defaults keep every existing class/field daily/history/no-sweep-shaped --
// bit-for-bit today's behaviour (migration stance).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn asset_class_cadence_columns_default_to_daily_shaped() {
    let pool = common::pool().await;
    let row: (String, i32, String) = sqlx::query_as(
        "INSERT INTO asset_class (name) VALUES ($1)
         RETURNING default_cadence, cadence_grace_days, identity_sweep")
        .bind(uniq("CadDflt")).fetch_one(&pool).await.unwrap();
    assert_eq!(row, ("daily".to_string(), 10, "none".to_string()),
        "existing classes must not be flipped into a sweep or non-daily cadence by the migration");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn field_def_cadence_columns_default_to_null_history() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar("INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadFDflt")).fetch_one(&pool).await.unwrap();
    let row: (Option<String>, String) = sqlx::query_as(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1, $2, 'Last', 'numeric') RETURNING cadence, fetch_via")
        .bind(class).bind(uniq("CADF3")).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (None, "history".to_string()),
        "a new field must default to no override and the history wire path");
}

// ---------------------------------------------------------------------------
// CHECK constraints
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn default_cadence_rejects_unknown_values() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, default_cadence) VALUES ($1, 'biweekly')")
        .bind(uniq("CadBad")).execute(&pool).await;
    assert!(err.is_err(), "unknown cadence values must be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn field_cadence_rejects_unknown_values() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar("INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadFBad")).fetch_one(&pool).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, cadence)
         VALUES ($1, $2, 'Last', 'numeric', 'biweekly')")
        .bind(class).bind(uniq("CADFB")).execute(&pool).await;
    assert!(err.is_err(), "unknown field-level cadence values must be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn cadence_grace_days_rejects_negative() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, cadence_grace_days) VALUES ($1, -1)")
        .bind(uniq("CadGraceBad")).execute(&pool).await;
    assert!(err.is_err(), "a negative grace period is meaningless");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn identity_sweep_rejects_unknown_values() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, identity_sweep) VALUES ($1, 'daily')")
        .bind(uniq("CadSweepBad")).execute(&pool).await;
    assert!(err.is_err(), "unknown identity_sweep values must be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn fetch_via_rejects_unknown_values() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar("INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadFetchBad")).fetch_one(&pool).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, fetch_via)
         VALUES ($1, $2, 'Last', 'numeric', 'streaming')")
        .bind(class).bind(uniq("CADFV")).execute(&pool).await;
    assert!(err.is_err(), "unknown fetch_via values must be rejected");
}

// ---------------------------------------------------------------------------
// CRUD used by the Settings editors (P9 pattern: registry::AssetClass /
// fields::FieldDef row structs + an update_* command per row kind).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn asset_class_cadence_capabilities_can_be_updated_and_read_back() {
    let pool = common::pool().await;
    let ac = registry::create_asset_class(&pool, &uniq("CadUpd"), "").await.unwrap();
    registry::update_asset_class_capabilities(&pool, ac.id, true, true, "factors", None,
        "monthly", 15, "market_status").await.unwrap();
    let all = registry::list_asset_classes(&pool).await.unwrap();
    let got = all.iter().find(|c| c.id == ac.id).unwrap();
    assert_eq!(got.default_cadence, "monthly");
    assert_eq!(got.cadence_grace_days, 15);
    assert_eq!(got.identity_sweep, "market_status");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn field_cadence_can_be_updated_and_read_back() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar("INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadFUpd")).fetch_one(&pool).await.unwrap();
    let field = fields::create_field(&pool, class, &uniq("CADFU"), "Last", "numeric",
        None, None, "", false, None, None).await.unwrap();
    assert_eq!(field.cadence, None);
    assert_eq!(field.fetch_via, "history");

    fields::update_field_cadence(&pool, field.id, Some("weekly"), "reference").await.unwrap();
    let all = fields::list_fields(&pool).await.unwrap();
    let got = all.iter().find(|f| f.id == field.id).unwrap();
    assert_eq!(got.cadence, Some("weekly".to_string()));
    assert_eq!(got.fetch_via, "reference");
}

// ---------------------------------------------------------------------------
// R1: schedule.identity_dow / last_identity_on mirror verify_dow /
// last_verified_on exactly (columns only -- Task 6 owns the scheduler logic).
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_identity_dow_defaults_off_and_accepts_a_weekday() {
    let pool = common::pool().await;
    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadSchedView")).fetch_one(&pool).await.unwrap();
    let row: (Option<i16>, Option<chrono::NaiveDate>) = sqlx::query_as(
        "INSERT INTO schedule (view_id) VALUES ($1)
         RETURNING identity_dow, last_identity_on")
        .bind(view).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (None, None), "identity_dow must default to off, unlike verify_dow's Friday-on default");

    let n = sqlx::query(
        "UPDATE schedule SET identity_dow = 3 WHERE view_id = $1")
        .bind(view).execute(&pool).await.unwrap();
    assert_eq!(n.rows_affected(), 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_identity_dow_rejects_out_of_range_values() {
    let pool = common::pool().await;
    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("CadSchedBad")).fetch_one(&pool).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO schedule (view_id, identity_dow) VALUES ($1, 8)")
        .bind(view).execute(&pool).await;
    assert!(err.is_err(), "identity_dow must respect the same 1..=7 ISO-weekday range as verify_dow");
}

// ===========================================================================
// Task 4 -- 11.4 fetch-when-due, 11.5 period-shaped gaps + the coverage
// predicate fix, 11.7 verify per cadence.
// ===========================================================================

fn d(s: &str) -> NaiveDate {
    s.parse().unwrap()
}

/// One class, one instrument, one view, one run row to hang observations off.
struct Fx {
    class: i64,
    instrument: i64,
    view: i64,
    security: String,
    run: i64,
}

async fn fixture(pool: &sqlx::PgPool, stem: &str, default_cadence: &str, grace: i32) -> Fx {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, default_cadence, cadence_grace_days)
         VALUES ($1,$2,$3) RETURNING id")
        .bind(uniq(stem)).bind(default_cadence).bind(grace)
        .fetch_one(pool).await.unwrap();
    let inst = store::create(pool).await.unwrap();
    let security = format!("{} Equity", uniq(stem));
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
    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(view).bind(inst.instrument_id).execute(pool).await.unwrap();
    // Backdated deliberately: `observation.run_id` is NOT NULL so the fixture
    // needs a run to hang prints off, but a run dated TODAY would spend the
    // view's once-a-day periodic attempt before any test had begun.
    let run: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, started_at)
         VALUES ($1,'eod','manual','ok','2000-01-03') RETURNING id")
        .bind(view).fetch_one(pool).await.unwrap();
    Fx { class, instrument: inst.instrument_id, view, security, run }
}

async fn add_field(pool: &sqlx::PgPool, fx: &Fx, mnemonic: &str,
                   cadence: Option<&str>, fetch_via: &str, value_kind: &str) -> i64 {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                cadence, fetch_via)
         VALUES ($1,$2,$2,$3,$4,$5) RETURNING id")
        .bind(fx.class).bind(mnemonic).bind(value_kind).bind(cadence).bind(fetch_via)
        .fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(fx.view).bind(id).execute(pool).await.unwrap();
    id
}

async fn add_obs(pool: &sqlx::PgPool, fx: &Fx, field: i64, on: NaiveDate, v: f64) {
    sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,$3,'raw',1,$4,$5)")
        .bind(fx.instrument).bind(field).bind(on).bind(v).bind(fx.run)
        .execute(pool).await.unwrap();
}

fn cfg(soft_limit: i64) -> PipelineConfig {
    PipelineConfig {
        data_dir: std::env::temp_dir(),
        python_path: "python".into(),
        script_path: "unused".into(),
        request_timeout_s: 5,
        soft_limit,
        blp_host: None,
        blp_port: None,
    }
}

/// Captures the wire plan the pipeline actually built, and returns nothing.
struct Recording(std::sync::Mutex<Vec<RequestSpec>>);

impl DataFetcher for Recording {
    async fn fetch(&self, req: &FetchRequest, _audit: Option<&Path>)
        -> AppResult<FetchOutcome> {
        *self.0.lock().unwrap() = fetch::plan_requests(req).unwrap();
        Ok(FetchOutcome::default())
    }
}

/// The sidecar must never be asked to fetch nothing (R6): its own
/// `validate_payload` rejects an empty `requests` list with a misleading
/// "payload has no 'requests'" error.
struct NeverFetch;

impl DataFetcher for NeverFetch {
    async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
        -> AppResult<FetchOutcome> {
        panic!("an empty plan must never reach the sidecar");
    }
}

/// Bloomberg answering with nothing at all -- no cells, no problems. That is
/// the shape of BOTH silences the quality gate has to tell apart: a daily name
/// the reply silently dropped (`quality_no_response`, what the gate is FOR),
/// and a periodic leg whose print has not published yet, which `blp_fetch.py`
/// returns as an empty `fieldData` with no exception at all.
struct SilentReply;

impl DataFetcher for SilentReply {
    async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
        -> AppResult<FetchOutcome> {
        Ok(FetchOutcome::default())
    }
}

// ---------------------------------------------------------------------------
// Fetch when due (11.4). The pure period arithmetic is unit-tested in
// scheduler.rs; these pin the database-facing behaviour.
// ---------------------------------------------------------------------------

/// (a) The most recently ENDED period has no print: exactly ONE ranged history
/// request, covering that period and nothing else, carrying MONTHLY.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_period_that_ended_without_a_print_is_due_as_one_ranged_monthly_request() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "DueMonthly", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    let today = d("2026-08-12");
    // June printed (dated its last trading day); July never did.
    add_obs(&pool, &fx, nav, d("2026-06-30"), 101.0).await;

    let legs = orchestrator::due_periodic_legs(&pool, fx.view, today).await.unwrap();
    assert_eq!(legs.len(), 1, "only the unprinted period is due: {legs:?}");
    assert_eq!((legs[0].start, legs[0].end), (d("2026-07-01"), d("2026-07-31")));
    assert_eq!(legs[0].cadence, "monthly");

    // ... and it plans as exactly one MONTHLY history spec (F3: one row).
    let req = FetchRequest {
        run_id: 1,
        assets: vec![fetch::FetchAsset {
            instrument_id: fx.instrument, asset_class_id: fx.class,
            class_name: "C".into(), label: fx.security.clone(),
            bdp_security: fx.security.clone() }],
        fields: vec![fetch::FetchField {
            field_id: nav, asset_class_id: fx.class,
            mnemonic: "FUND_NET_ASSET_VAL".into(), value_kind: "numeric".into(),
            cadence: "monthly".into(), fetch_via: "history".into() }],
        start: today, end: today,
        periodic: legs,
    };
    let plan = fetch::plan_requests(&req).unwrap();
    assert_eq!(plan.len(), 1, "one period, one request: {plan:?}");
    assert_eq!(plan[0].kind, "history");
    assert_eq!(plan[0].periodicity.as_deref(), Some("MONTHLY"),
               "R3: a periodic history spec MUST carry its periodicity");
    assert_eq!(plan[0].start.as_deref(), Some("20260701"));
    assert_eq!(plan[0].end.as_deref(), Some("20260731"));
    assert_eq!(plan[0].fields, vec!["FUND_NET_ASSET_VAL"]);
}

/// (b) The print landed: nothing is due, and no hit is spent.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_period_whose_print_arrived_is_not_due() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "DueDone", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    let today = d("2026-08-12");
    add_obs(&pool, &fx, nav, d("2026-06-30"), 101.0).await;
    add_obs(&pool, &fx, nav, d("2026-07-31"), 102.0).await;

    let legs = orchestrator::due_periodic_legs(&pool, fx.view, today).await.unwrap();
    assert!(legs.is_empty(), "both completed periods printed: {legs:?}");
}

/// (c) F3: the in-progress period has no row to fetch, so it is never asked
/// for -- not on the first of the month, not on the last day of it.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_unfinished_current_period_is_never_fetched() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "DueOpen", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    add_obs(&pool, &fx, nav, d("2026-06-30"), 101.0).await;
    add_obs(&pool, &fx, nav, d("2026-07-31"), 102.0).await;

    for today in ["2026-08-01", "2026-08-12", "2026-08-31"] {
        let legs = orchestrator::due_periodic_legs(&pool, fx.view, d(today)).await.unwrap();
        assert!(legs.is_empty(),
                "August is unfinished on {today}; asking for it buys a row that \
                 does not exist: {legs:?}");
    }
    // The day after it ends, it becomes due.
    let legs = orchestrator::due_periodic_legs(&pool, fx.view, d("2026-09-01")).await.unwrap();
    assert_eq!(legs.len(), 1);
    assert_eq!((legs[0].start, legs[0].end), (d("2026-08-01"), d("2026-08-31")));
}

// ---------------------------------------------------------------------------
// (d) 11.5 the coverage predicate: `expected` counts ONLY daily x history
// non-text fields. THE consequence: a date is never permanently uncovered
// because of a field daily backfill could never supply -- which is what armed
// the P10-review "permanently-partial days re-bought every day" defect.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_missing_monthly_field_does_not_make_a_day_permanently_uncovered() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "CovMixed", "daily", 10).await;
    let px = add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    // A monthly NAV on the same class: absent on every weekday, by design.
    add_field(&pool, &fx, "FUND_NET_ASSET_VAL", Some("monthly"), "history", "numeric").await;
    // ... and a reference-via yield: absent from any backfill, by design (F6).
    add_field(&pool, &fx, "YLD_YTM_MID", None, "reference", "numeric").await;

    let day = d("2026-08-18"); // Tuesday
    add_obs(&pool, &fx, px, day, 100.0).await;

    let gaps = scheduler::detect_gaps(&pool, fx.view, 1, d("2026-08-19")).await.unwrap();
    let daily: Vec<_> = gaps.iter().filter(|g| g.period.is_none()).collect();
    assert!(daily.iter().all(|g| !(g.start <= day && day <= g.end)),
            "the day's only daily x history field is present, so the day is covered; \
             a monthly and a reference-via field can never make it otherwise: {gaps:?}");
}

// ---------------------------------------------------------------------------
// (e) 11.5 period-shaped gaps: grace, and a 2-completed-period lookback.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_period_gap_waits_for_grace_and_looks_back_exactly_two_cycles() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "PerGap", "monthly", 10).await;
    add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;

    // 2026-08-05: July ended 5 days ago, inside the 10-day grace -- while June,
    // 36 days past its end, is long overdue. Grace is judged per period, not
    // per view, which is the whole point of hanging it off the period end.
    let early = scheduler::detect_gaps(&pool, fx.view, 10, d("2026-08-05")).await.unwrap();
    let early_periods: Vec<&str> = early.iter().filter_map(|g| g.period.as_deref()).collect();
    assert_eq!(early_periods, vec!["2026-06"],
               "inside grace a late July NAV is not yet anomalous: {early:?}");

    // 2026-08-15: 15 days past July's end -- overdue.
    let late = scheduler::detect_gaps(&pool, fx.view, 10, d("2026-08-15")).await.unwrap();
    let periods: Vec<&str> = late.iter().filter_map(|g| g.period.as_deref()).collect();
    assert_eq!(periods, vec!["2026-07", "2026-06"],
               "exactly the two completed periods in the lookback, newest first, \
                both past grace -- May is out of the window: {late:?}");
    let july = late.iter().find(|g| g.period.as_deref() == Some("2026-07")).unwrap();
    assert_eq!((july.start, july.end), (d("2026-07-01"), d("2026-07-31")),
               "a period gap carries its real period bounds, not a fake day range");
    assert_eq!(july.instrument_id, fx.instrument);
}

// ---------------------------------------------------------------------------
// (f) 11.7 verify per cadence.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn verify_reads_five_weekdays_daily_and_two_completed_periods_monthly() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "VerMix", "daily", 10).await;
    add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    add_field(&pool, &fx, "FUND_NET_ASSET_VAL", Some("monthly"), "history", "numeric").await;
    add_field(&pool, &fx, "YLD_YTM_MID", None, "reference", "numeric").await;
    add_field(&pool, &fx, "CAPITAL_ACCOUNT", Some("irregular"), "history", "numeric").await;

    let end = d("2026-08-14"); // Friday
    let start = scheduler::verify_window_start(end);
    let rec = Recording(std::sync::Mutex::new(Vec::new()));
    let out = orchestrator::run_verify_with(&pool, &cfg(1_000_000), &rec, fx.view, start, end)
        .await.unwrap();
    assert!(matches!(out, RunOutcome::Completed { .. }), "{out:?}");
    let plan = rec.0.into_inner().unwrap();

    let daily: Vec<_> = plan.iter().filter(|s| s.periodicity.is_none()).collect();
    assert_eq!(daily.len(), 1, "one daily leg: {plan:?}");
    assert_eq!(daily[0].fields, vec!["PX_LAST"],
               "reference-via and irregular fields have nothing past to re-read");
    assert_eq!(daily[0].start.as_deref(), Some("20260810"), "5 weekdays back");
    assert_eq!(daily[0].end.as_deref(), Some("20260814"));

    let periodic: Vec<_> = plan.iter().filter(|s| s.periodicity.is_some()).collect();
    assert_eq!(periodic.len(), 1, "one monthly leg: {plan:?}");
    assert_eq!(periodic[0].periodicity.as_deref(), Some("MONTHLY"));
    assert_eq!(periodic[0].fields, vec!["FUND_NET_ASSET_VAL"]);
    let two = scheduler::completed_periods(chrono::Local::now().date_naive(), "monthly", 2);
    assert_eq!(periodic[0].start.as_deref(),
               Some(two[1].0.format("%Y%m%d").to_string().as_str()),
               "the verify leg re-reads the last TWO completed periods");
    assert_eq!(periodic[0].end.as_deref(),
               Some(two[0].1.format("%Y%m%d").to_string().as_str()));

    assert!(plan.iter().all(|s| !s.fields.iter().any(|f| f == "YLD_YTM_MID")),
            "a reference snapshot cannot re-read the past: {plan:?}");
    assert!(plan.iter().all(|s| !s.fields.iter().any(|f| f == "CAPITAL_ACCOUNT")),
            "an irregular field has no period to re-read: {plan:?}");
}

/// Dropping the unverifiable fields must not drop a whole CLASS into
/// `plan_requests`' "no fields configured" error: a bond class priced entirely
/// through `fetch_via = 'reference'` (probe F6/F7) sharing a view with an
/// equity class would otherwise fail the WHOLE week's verify for both, and
/// burn one of the three daily scheduled attempts doing it.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_reference_only_class_does_not_fail_the_whole_views_verify() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "VerRefCls", "daily", 10).await;
    add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;

    // A second class in the same view whose every field is a snapshot.
    let bonds: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("VerRefBond")).fetch_one(&pool).await.unwrap();
    let bond = store::create(&pool).await.unwrap();
    let bond_security = format!("{} Govt", uniq("CT10"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, bond.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: bond_security.clone(),
        exch_code: None, valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(bond.instrument_id).bind(bonds).bind(&bond_security)
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(fx.view).bind(bond.instrument_id).execute(&pool).await.unwrap();
    let yld: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, fetch_via)
         VALUES ($1,'YLD_YTM_MID','Yield','numeric','reference') RETURNING id")
        .bind(bonds).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(fx.view).bind(yld).execute(&pool).await.unwrap();

    let end = d("2026-08-14");
    let rec = Recording(std::sync::Mutex::new(Vec::new()));
    let out = orchestrator::run_verify_with(&pool, &cfg(1_000_000), &rec, fx.view,
                                            scheduler::verify_window_start(end), end)
        .await.expect("a class with nothing to verify must not fail the verify");
    assert!(matches!(out, RunOutcome::Completed { .. }), "{out:?}");

    let plan = rec.0.into_inner().unwrap();
    assert_eq!(plan.len(), 1, "only the equity class has anything to re-read: {plan:?}");
    assert_eq!(plan[0].fields, vec!["PX_LAST"]);
    assert!(!plan[0].securities.contains(&bond_security),
            "the bond leaves the run with its fields: {plan:?}");
}

/// The same rule taken to its end: a view where NOTHING can be re-read is a
/// clean no-op week, not a failure -- and it still writes a run row, or the
/// slot would fire again on the next heartbeat.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_view_with_nothing_verifiable_completes_without_fetching() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "VerNothing", "daily", 10).await;
    add_field(&pool, &fx, "YLD_YTM_MID", None, "reference", "numeric").await;
    add_field(&pool, &fx, "CAPITAL_ACCOUNT", Some("irregular"), "history", "numeric").await;

    let end = d("2026-08-14");
    let out = orchestrator::run_verify_with(&pool, &cfg(1_000_000), &NeverFetch, fx.view,
                                            scheduler::verify_window_start(end), end)
        .await.unwrap();
    let RunOutcome::Completed { run_id, summary, .. } = out else {
        panic!("a week with nothing to verify must still complete");
    };
    assert_eq!(summary.inserted, 0);
    let (status, hits): (String, i64) = sqlx::query_as(
        "SELECT status, estimated_hits FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!((status.as_str(), hits), ("ok", 0),
               "the run row exists and is closed, so the slot is spent");
}

// ---------------------------------------------------------------------------
// R6: an all-periodic view with nothing due plans zero requests. That is the
// permanent mid-month state of such a view, not a transient one.
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_eod_run_with_nothing_to_fetch_is_a_clean_no_op() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "EmptyPlan", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    // Both completed periods printed: nothing is due today, whatever today is.
    for (_, e) in scheduler::completed_periods(chrono::Local::now().date_naive(), "monthly", 2) {
        add_obs(&pool, &fx, nav, e, 100.0).await;
    }

    let out = orchestrator::run_eod_with(&pool, &cfg(1_000_000), &NeverFetch, fx.view,
                                         "manual", d("2026-08-18"), true).await.unwrap();
    let RunOutcome::Completed { run_id, summary, quality_findings, .. } = out else {
        panic!("an empty plan must still complete");
    };
    assert_eq!(summary.inserted, 0);
    assert_eq!(quality_findings, 0,
               "nothing was requested, so nothing failed to answer");
    let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "ok", "a quiet day is a clean day, not a partial one");
}

/// A run that asked for no DAILY history asked nothing about trading
/// sessions, so nothing it brings back is evidence about them. Without this,
/// Rule B would read one MONTHLY row as proof that every weekday of the run's
/// window was closed -- ~240 fake holidays a year for exactly the funds P11
/// exists to support. (Task 5 gates this per field inside `ingest`; this pins
/// the planner-level half the orchestrator can settle by itself.)
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_periodic_only_run_records_no_non_trading_evidence() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "PerNoEvid", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    let period = scheduler::completed_periods(chrono::Local::now().date_naive(), "monthly", 1)[0];

    // The monthly leg comes back with a dated no_data -- Rule A's exact input.
    struct SilentMonth(i64, i64, NaiveDate);
    impl DataFetcher for SilentMonth {
        async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            Ok(FetchOutcome {
                cells: vec![],
                problems: vec![getbloomdata_lib::fetch::CellProblem {
                    instrument_id: Some(self.0), field_id: Some(self.1),
                    obs_date: Some(self.2), code: "no_data".into(),
                    detail: "no data".into() }],
            })
        }
    }
    let out = orchestrator::run_eod_with(
        &pool, &cfg(1_000_000), &SilentMonth(fx.instrument, nav, period.1),
        fx.view, "manual", d("2026-08-18"), true).await.unwrap();
    assert!(matches!(out, RunOutcome::Completed { .. }), "{out:?}");

    let marks: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM non_trading_day WHERE instrument_id = $1")
        .bind(fx.instrument).fetch_one(&pool).await.unwrap();
    assert_eq!(marks, 0,
               "a periodic fetch's silence is not a holiday -- the series simply                 does not print daily");
}

/// The once-per-day cap survives a restart because it is read off `run` rows,
/// not held in memory (controller ruling; the gap-backfill idiom).
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_second_run_the_same_day_does_not_re_buy_the_periodic_leg() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "OncePerDay", "monthly", 10).await;
    add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    let today = chrono::Local::now().date_naive();

    assert!(!orchestrator::periodic_attempted_today(&pool, fx.view, today).await.unwrap());
    assert!(!orchestrator::due_periodic_legs(&pool, fx.view, today).await.unwrap().is_empty(),
            "nothing has ever printed, so the completed periods are due");

    sqlx::query("INSERT INTO run (view_id, kind, trigger_kind, status)
                 VALUES ($1,'eod','scheduled','failed')")
        .bind(fx.view).execute(&pool).await.unwrap();
    assert!(orchestrator::periodic_attempted_today(&pool, fx.view, today).await.unwrap(),
            "a FAILED attempt still spends the day -- the gap-backfill doctrine");
}

// ===========================================================================
// 11.6 Evidence honesty: gating, the NIL-streak alarm, publication_overdue.
//
// All three exist because of probe finding F6: an unentitled bond returned NIL
// for every weekday of the capture, and the pre-P11 rules would have written
// eight `non_trading_day` rows -- an entitlement hole permanently disguised as
// a run of holidays, with zero alerts.
// ===========================================================================

/// A second instrument in the fixture's class, view and book -- so a test can
/// say "this security is inside the periodic leg and that one is not" about
/// the SAME date.
async fn add_instrument(pool: &sqlx::PgPool, fx: &Fx, stem: &str) -> i64 {
    let inst = store::create(pool).await.unwrap();
    let security = format!("{} Equity", uniq(stem));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: security.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(inst.instrument_id).bind(fx.class).bind(&security)
        .execute(pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(fx.view).bind(inst.instrument_id).execute(pool).await.unwrap();
    inst.instrument_id
}

/// A fresh run row to hang one leg of a multi-run scenario off. Backdated for
/// the same reason `fixture` backdates its own (the periodic once-a-day cap).
async fn new_run(pool: &sqlx::PgPool, fx: &Fx) -> i64 {
    sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, started_at)
         VALUES ($1,'eod','manual','ok','2000-01-03') RETURNING id")
        .bind(fx.view).fetch_one(pool).await.unwrap()
}

fn asset_of(fx: &Fx, instrument: i64) -> FetchAsset {
    FetchAsset {
        instrument_id: instrument,
        asset_class_id: fx.class,
        class_name: "Fixture".into(),
        label: fx.security.clone(),
        bdp_security: fx.security.clone(),
    }
}

/// Bloomberg answering "nothing here" for each of `days` -- Rule A's exact
/// input, and what an unentitled series returns on every single weekday (F6).
fn nil_outcome(instrument: i64, field: i64, days: &[NaiveDate]) -> FetchOutcome {
    FetchOutcome {
        cells: vec![],
        problems: days.iter().map(|&on| CellProblem {
            instrument_id: Some(instrument), field_id: Some(field),
            obs_date: Some(on), code: "no_data".into(), detail: "NIL".into(),
        }).collect(),
    }
}

async fn issues_of(pool: &sqlx::PgPool, run: i64, code: &str) -> Vec<(String, String)> {
    sqlx::query_as("SELECT severity, detail FROM ingest_issue
                     WHERE run_id = $1 AND code = $2 ORDER BY id")
        .bind(run).bind(code).fetch_all(pool).await.unwrap()
}

async fn marks_of(pool: &sqlx::PgPool, instrument: i64) -> Vec<NaiveDate> {
    sqlx::query_scalar("SELECT obs_date FROM non_trading_day
                         WHERE instrument_id = $1 ORDER BY obs_date")
        .bind(instrument).fetch_all(pool).await.unwrap()
}

/// (a) A periodic leg's absent days are not holiday evidence. A monthly series
/// prints once; the other twenty weekdays of the month are silent BY DESIGN,
/// and recording them would both fabricate ~240 fake holidays a year and
/// permanently suppress the period's real gap.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_periodic_legs_absent_days_are_never_non_trading_evidence() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "PerShadow", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    let (ps, pe) = (d("2026-07-01"), d("2026-07-31"));

    let req = FetchRequest {
        run_id: fx.run,
        assets: vec![asset_of(&fx, fx.instrument)],
        fields: vec![FetchField {
            field_id: nav, asset_class_id: fx.class,
            mnemonic: "FUND_NET_ASSET_VAL".into(), value_kind: "numeric".into(),
            cadence: "monthly".into(), fetch_via: "history".into() }],
        start: d("2026-08-18"), end: d("2026-08-18"),
        periodic: vec![PeriodicLeg {
            cadence: "monthly".into(), start: ps, end: pe,
            instrument_ids: vec![fx.instrument], field_ids: vec![nav] }],
    };
    // The month came back with a dated NIL on three of its weekdays.
    let out = nil_outcome(fx.instrument, nav,
                          &[d("2026-07-15"), d("2026-07-30"), pe]);

    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 0, "silence inside a period is expected, not a holiday");
    assert!(marks_of(&pool, fx.instrument).await.is_empty());
}

/// The binding mixed-view case: one request carries a daily leg AND a periodic
/// leg. The gate is per (instrument, date), derived from the legs themselves --
/// the daily leg's own silence is still recorded, and a date inside a periodic
/// leg's range is still recorded for an instrument that leg does not cover.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_mixed_request_gates_evidence_per_instrument_and_date() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "MixEvid", "daily", 10).await;
    let px = add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", Some("monthly"),
                        "history", "numeric").await;
    // A second name in the same view that the periodic leg does NOT cover.
    let daily_only = add_instrument(&pool, &fx, "MixDaily").await;
    let (ps, pe) = (d("2026-07-01"), d("2026-07-31"));
    let day = d("2026-08-18"); // Tuesday, the run's own target

    let req = FetchRequest {
        run_id: fx.run,
        assets: vec![asset_of(&fx, fx.instrument), asset_of(&fx, daily_only)],
        fields: vec![
            FetchField::daily_history(px, fx.class, "PX_LAST", "numeric"),
            FetchField {
                field_id: nav, asset_class_id: fx.class,
                mnemonic: "FUND_NET_ASSET_VAL".into(), value_kind: "numeric".into(),
                cadence: "monthly".into(), fetch_via: "history".into() },
        ],
        start: day, end: day,
        periodic: vec![PeriodicLeg {
            cadence: "monthly".into(), start: ps, end: pe,
            instrument_ids: vec![fx.instrument], field_ids: vec![nav] }],
    };
    // Both ENDS of the leg's range, so the boundary is pinned, not just the
    // middle of it.
    let mut out = nil_outcome(fx.instrument, nav, &[ps, pe]);           // inside the leg
    out.problems.extend(nil_outcome(daily_only, nav, &[pe]).problems);  // same date, no leg
    out.problems.extend(nil_outcome(fx.instrument, px, &[day]).problems);
    out.problems.extend(nil_outcome(daily_only, px, &[day]).problems);

    ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();

    assert_eq!(marks_of(&pool, fx.instrument).await, vec![day],
               "the periodic leg's date is shadowed for the instrument it covers; \
                the daily leg's own silence is still evidence");
    assert_eq!(marks_of(&pool, daily_only).await, vec![pe, day],
               "an instrument outside the leg is not shadowed by someone else's period");
}

/// (b) Five consecutive all-NIL weekdays -- assembled across two runs, exactly
/// as an entitlement hole assembles it in production -- raise ONE `nil_streak`
/// quality finding, and the evidence rows stay (spec 11.6: the alarm is the
/// human's signal, the evidence is what stops the machine re-buying junk).
#[tokio::test]
#[ignore = "requires postgres"]
async fn five_all_nil_weekdays_across_two_runs_raise_exactly_one_nil_streak() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "NilStreak", "daily", 10).await;
    let px = add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    let (mon, tue, wed, thu, fri) = (d("2026-08-17"), d("2026-08-18"), d("2026-08-19"),
                                     d("2026-08-20"), d("2026-08-21"));
    let req_of = |run: i64, start: NaiveDate, end: NaiveDate| FetchRequest {
        run_id: run,
        assets: vec![asset_of(&fx, fx.instrument)],
        fields: vec![FetchField::daily_history(px, fx.class, "PX_LAST", "numeric")],
        start, end, periodic: vec![],
    };

    // Run 1: a four-weekday range, NIL on every day of it.
    let r1 = new_run(&pool, &fx).await;
    let req1 = req_of(r1, mon, thu);
    let out1 = nil_outcome(fx.instrument, px, &[mon, tue, wed, thu]);
    ingest::record_non_trading_days(&pool, &req1, &out1).await.unwrap();
    quality::run_quality_gate(&pool, r1, &req1, &out1).await.unwrap();
    assert!(issues_of(&pool, r1, "nil_streak").await.is_empty(),
            "four weekdays is under the threshold -- a long weekend is real");

    // Run 2: Friday is the fifth.
    let r2 = new_run(&pool, &fx).await;
    let req2 = req_of(r2, fri, fri);
    let out2 = nil_outcome(fx.instrument, px, &[fri]);
    ingest::record_non_trading_days(&pool, &req2, &out2).await.unwrap();
    let n = quality::run_quality_gate(&pool, r2, &req2, &out2).await.unwrap();
    assert!(n >= 1, "the streak finding counts toward the run's quality findings");

    let found = issues_of(&pool, r2, "nil_streak").await;
    assert_eq!(found.len(), 1, "once per run, per instrument: {found:?}");
    assert_eq!(found[0].0, "quality", "P7 severity, not a warn");
    assert!(found[0].1.contains("2026-08-17") && found[0].1.contains("2026-08-21"),
            "the detail names the span: {}", found[0].1);

    assert_eq!(marks_of(&pool, fx.instrument).await, vec![mon, tue, wed, thu, fri],
               "the evidence is STILL recorded -- it is what stops the auto-backfill \
                re-buying an unentitled series every day");
}

/// (c) Four is not five, and five weekdays with a hole in the middle are not a
/// streak either: the run has to be CONSECUTIVE, or a fortnight of scattered
/// real holidays would cry wolf.
#[tokio::test]
#[ignore = "requires postgres"]
async fn four_consecutive_nil_weekdays_stay_quiet() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "NilShort", "daily", 10).await;
    let px = add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    // Tue..Fri NIL (four), plus the PREVIOUS Friday -- five marks, but Monday
    // in between traded, so the trailing run is four.
    let days = [d("2026-08-14"), d("2026-08-18"), d("2026-08-19"),
                d("2026-08-20"), d("2026-08-21")];
    let run = new_run(&pool, &fx).await;
    let req = FetchRequest {
        run_id: run,
        assets: vec![asset_of(&fx, fx.instrument)],
        fields: vec![FetchField::daily_history(px, fx.class, "PX_LAST", "numeric")],
        start: d("2026-08-21"), end: d("2026-08-21"), periodic: vec![],
    };
    let out = nil_outcome(fx.instrument, px, &days);
    ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    quality::run_quality_gate(&pool, run, &req, &out).await.unwrap();

    assert!(issues_of(&pool, run, "nil_streak").await.is_empty(),
            "2026-08-17 traded, so the trailing run is four weekdays, not five");
    assert_eq!(marks_of(&pool, fx.instrument).await.len(), 5);
}

/// (d) A period late past grace is a `publication_overdue` quality finding
/// naming the period -- "the June NAV never arrived", instead of a month of
/// day-shaped gap noise.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_overdue_period_is_a_publication_overdue_finding_naming_the_period() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "PubOverdue", "monthly", 10).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    add_obs(&pool, &fx, nav, d("2026-06-30"), 101.0).await; // June printed; July never did

    // Inside grace (July ended 07-31, grace 10 -> 08-10) nothing is anomalous.
    let quiet = new_run(&pool, &fx).await;
    let n = quality::record_publication_overdue(&pool, quiet, fx.view, d("2026-08-09"))
        .await.unwrap();
    assert_eq!(n, 0, "a late print inside grace is not yet late enough to alarm");

    let run = new_run(&pool, &fx).await;
    let n = quality::record_publication_overdue(&pool, run, fx.view, d("2026-08-12"))
        .await.unwrap();
    assert_eq!(n, 1, "exactly the one period that is past grace and unprinted");

    let found = issues_of(&pool, run, "publication_overdue").await;
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, "quality");
    assert!(found[0].1.contains("2026-07"),
            "the finding names the period, not a fake day range: {}", found[0].1);
    assert!(found[0].1.contains("FUND_NET_ASSET_VAL"),
            "and the field that failed to print: {}", found[0].1);
}

/// ... and it is wired into every run, AFTER ingest: the period this run
/// actually bought is not reported late, and the one it could not buy is.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_run_flags_the_period_it_could_not_buy_and_not_the_one_it_just_did() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "PubWire", "monthly", 0).await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;
    let today = chrono::Local::now().date_naive();
    let periods = scheduler::completed_periods(today, "monthly", 2);
    let (newest, older) = (periods[0], periods[1]);

    // The newest period prints in this very run; the older one stays missing.
    struct OneMonth(i64, i64, NaiveDate);
    impl DataFetcher for OneMonth {
        async fn fetch(&self, _req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            Ok(FetchOutcome {
                cells: vec![ObsCell { instrument_id: self.0, field_id: self.1,
                                      obs_date: self.2, value: CellValue::Num(103.5) }],
                problems: vec![],
            })
        }
    }
    let out = orchestrator::run_eod_with(
        &pool, &cfg(1_000_000), &OneMonth(fx.instrument, nav, newest.1),
        fx.view, "manual", d("2026-08-18"), true).await.unwrap();
    let RunOutcome::Completed { run_id, quality_findings, .. } = out else {
        panic!("{out:?}");
    };
    assert_eq!(quality_findings, 1,
               "one overdue period, and nothing else to say about this run");

    let found = issues_of(&pool, run_id, "publication_overdue").await;
    assert_eq!(found.len(), 1, "{found:?}");
    let label = scheduler::period_label(older.0, "monthly");
    assert!(found[0].1.contains(&label), "expected {label} in: {}", found[0].1);
    assert!(!found[0].1.contains(&scheduler::period_label(newest.0, "monthly")),
            "the period this run just bought is not late: {}", found[0].1);

    let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "partial",
               "a quality finding makes the run partial -- the standing P7 rule, \
                and publication_overdue does nothing beyond it");
}

/// Review finding: the shadow must be per FIELD, not just per instrument and
/// date. Daily and periodic ranges overlap routinely -- the first verify of any
/// month, an EOD on the first business day of a month, every Monday under a
/// weekly cadence -- and on those days the daily leg's holiday evidence is
/// exactly what must survive. Losing it would leave `detect_gaps` staring at a
/// permanently uncovered date and re-buying it daily.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_daily_holiday_inside_a_periodic_legs_range_is_still_evidence() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "OverlapEvid", "daily", 10).await;
    let px = add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", Some("monthly"),
                        "history", "numeric").await;
    // The verify shape: a trailing daily window sitting INSIDE the periodic
    // leg's two-period span.
    let (ps, pe) = (d("2026-07-01"), d("2026-07-31"));
    let (holiday, nil_fill) = (d("2026-07-03"), d("2026-07-06")); // Fri, Mon

    let req = FetchRequest {
        run_id: fx.run,
        assets: vec![asset_of(&fx, fx.instrument)],
        fields: vec![
            FetchField::daily_history(px, fx.class, "PX_LAST", "numeric"),
            FetchField {
                field_id: nav, asset_class_id: fx.class,
                mnemonic: "FUND_NET_ASSET_VAL".into(), value_kind: "numeric".into(),
                cadence: "monthly".into(), fetch_via: "history".into() },
        ],
        start: d("2026-07-01"), end: d("2026-07-07"),
        periodic: vec![PeriodicLeg {
            cadence: "monthly".into(), start: ps, end: pe,
            instrument_ids: vec![fx.instrument], field_ids: vec![nav] }],
    };
    let mut out = nil_outcome(fx.instrument, px, &[holiday]);
    // The sidecar's NIL-fill row carries NO field (blp_fetch.py) -- and it is
    // only ever produced for a DAILY request, so it is daily evidence too.
    out.problems.push(CellProblem {
        instrument_id: Some(fx.instrument), field_id: None,
        obs_date: Some(nil_fill), code: "no_data".into(),
        detail: "non-trading day (NIL fill)".into() });
    // ... and the monthly leg's own silence on a third day inside the range.
    out.problems.extend(nil_outcome(fx.instrument, nav, &[d("2026-07-02")]).problems);
    // One real daily print, so rule B has something to infer from.
    out.cells.push(ObsCell { instrument_id: fx.instrument, field_id: px,
                             obs_date: d("2026-07-07"), value: CellValue::Num(12.0) });

    ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();

    let marks = marks_of(&pool, fx.instrument).await;
    assert!(marks.contains(&holiday),
            "the DAILY field's holiday survives a periodic leg spanning the same \
             dates -- otherwise detect_gaps re-buys it every day: {marks:?}");
    assert!(marks.contains(&nil_fill),
            "a field-less NIL-fill row is daily evidence too: {marks:?}");
    assert!(!marks.contains(&d("2026-07-02")),
            "the monthly field's own silence inside its period is still shadowed: {marks:?}");
    assert!(marks.contains(&d("2026-07-01")),
            "and rule B's inference survives the overlap too: the daily field \
             answered on 07-07 and was silent on 07-01: {marks:?}");
}

/// Review finding: the cells-map exclusion had no test -- deleting it left the
/// suite green. Without it, ONE monthly NAV cell satisfies rule B's "this
/// instrument answered elsewhere in the range" and fabricates a holiday on
/// every silent weekday of a ranged run.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_periodic_cell_is_not_proof_that_the_daily_leg_answered() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "PerCellB", "daily", 10).await;
    let px = add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;
    let nav = add_field(&pool, &fx, "FUND_NET_ASSET_VAL", Some("monthly"),
                        "history", "numeric").await;
    let (ps, pe) = (d("2026-07-01"), d("2026-07-31"));

    let req = FetchRequest {
        run_id: fx.run,
        assets: vec![asset_of(&fx, fx.instrument)],
        fields: vec![
            FetchField::daily_history(px, fx.class, "PX_LAST", "numeric"),
            FetchField {
                field_id: nav, asset_class_id: fx.class,
                mnemonic: "FUND_NET_ASSET_VAL".into(), value_kind: "numeric".into(),
                cadence: "monthly".into(), fetch_via: "history".into() },
        ],
        start: ps, end: pe,
        periodic: vec![PeriodicLeg {
            cadence: "monthly".into(), start: ps, end: pe,
            instrument_ids: vec![fx.instrument], field_ids: vec![nav] }],
    };
    // The month's ONLY answer is the NAV print. The daily field said nothing at
    // all, which is a `quality_no_response` question, not twenty-two holidays.
    let out = FetchOutcome {
        cells: vec![ObsCell { instrument_id: fx.instrument, field_id: nav,
                              obs_date: pe, value: CellValue::Num(100.0) }],
        problems: vec![],
    };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();

    assert_eq!(n, 0, "a monthly print is not proof the daily leg answered");
    assert!(marks_of(&pool, fx.instrument).await.is_empty(),
            "rule B must not infer a month of holidays from one NAV cell");
}

/// Final-review finding. `quality_no_response` is a statement about the run's
/// WIRE PLAN -- "we named this security and Bloomberg answered neither way" --
/// and pre-P11 that was the same thing as the view, because every asset rode
/// every run. 11.4 broke the equivalence: a periodic class mid-period is
/// planned by nothing.
///
/// Judged against the view, a mixed view (daily equities beside a monthly fund
/// class) would file one bogus finding per fund member PER EOD RUN on every
/// non-due day, land every one of those runs 'partial', and grow
/// `ingest_issue` without bound -- precisely the daily noise 11.6 exists to
/// remove. Periodic silence is judged by `publication_overdue`, after grace,
/// and by nothing else.
///
/// The gate's original purpose is pinned in the SAME run: the daily equity
/// Bloomberg silently dropped still gets its finding.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_class_the_plan_never_named_is_not_judged_by_quality_no_response() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "MixNoResp", "daily", 10).await;
    add_field(&pool, &fx, "PX_LAST", None, "history", "numeric").await;

    // A monthly fund class sharing the view, with EVERY completed period
    // already printed -- so nothing of it is due and the plan names none of
    // its members, while the equity leg keeps the plan non-empty.
    let funds: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, default_cadence, cadence_grace_days)
         VALUES ($1,'monthly',10) RETURNING id")
        .bind(uniq("MixFundCls")).fetch_one(&pool).await.unwrap();
    let fund = store::create(&pool).await.unwrap();
    let fund_security = format!("{} Equity", uniq("MIXFUND"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, fund.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: fund_security.clone(),
        exch_code: None, valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(fund.instrument_id).bind(funds).bind(&fund_security)
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(fx.view).bind(fund.instrument_id).execute(&pool).await.unwrap();
    let nav: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'FUND_NET_ASSET_VAL','NAV','numeric') RETURNING id")
        .bind(funds).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(fx.view).bind(nav).execute(&pool).await.unwrap();
    for (_, e) in scheduler::completed_periods(chrono::Local::now().date_naive(), "monthly", 2) {
        sqlx::query(
            "INSERT INTO observation
               (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
             VALUES ($1,$2,$3,'raw',1,100,$4)")
            .bind(fund.instrument_id).bind(nav).bind(e).bind(fx.run)
            .execute(&pool).await.unwrap();
    }

    let out = orchestrator::run_eod_with(&pool, &cfg(1_000_000), &SilentReply, fx.view,
                                         "manual", d("2026-08-18"), true).await.unwrap();
    let RunOutcome::Completed { run_id, .. } = out else {
        panic!("a mixed view's EOD run must complete: {out:?}");
    };

    let flagged: Vec<i64> = sqlx::query_scalar(
        "SELECT instrument_id FROM ingest_issue
          WHERE run_id = $1 AND code = 'quality_no_response'
            AND instrument_id IS NOT NULL
          ORDER BY instrument_id")
        .bind(run_id).fetch_all(&pool).await.unwrap();
    assert_eq!(flagged, vec![fx.instrument],
               "the wire-planned daily name Bloomberg dropped is still flagged, and \
                the fund the plan never named is not");
}

/// The same doctrine on the other side of the partition. A DUE periodic leg
/// whose print has not published yet comes back SILENT -- the sidecar returns
/// an empty `fieldData` for the whole multi-day range, with no per-cell
/// exception to explain it -- so "requested, no answer" would be true of that
/// instrument every single day until the print lands, bypassing the grace that
/// `publication_overdue` exists to enforce.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_due_periodic_leg_that_has_not_printed_yet_is_not_a_no_response() {
    let pool = common::pool().await;
    let fx = fixture(&pool, "DueSilent", "monthly", 10).await;
    add_field(&pool, &fx, "FUND_NET_ASSET_VAL", None, "history", "numeric").await;

    // Nothing has ever printed, so the completed periods ARE due: the plan is
    // non-empty (one MONTHLY ranged request) and the gate runs.
    let out = orchestrator::run_eod_with(&pool, &cfg(1_000_000), &SilentReply, fx.view,
                                         "manual", d("2026-08-18"), true).await.unwrap();
    let RunOutcome::Completed { run_id, .. } = out else {
        panic!("an all-periodic run must complete: {out:?}");
    };

    let no_response: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM ingest_issue
          WHERE run_id = $1 AND code = 'quality_no_response'")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(no_response, 0,
               "an unpublished print is late, not silently dropped -- that verdict \
                is publication_overdue's to make, once, after grace");
}

// ===========================================================================
// Task 6 -- 11.8 the weekly identity sweep. Fetch + dispatch only: the
// retirement machinery itself is P9's (`lifecycle::retire_path` / the M&A
// investigation, routed by `asset_class.ma_capable`), and these tests assert
// that the sweep *reaches* it, not that it reimplements it.
// ===========================================================================

/// A `MasterFetcher` that replays canned sweep answers AND charges the ledger
/// the way the live wire seam does.
///
/// `MockMasterFetcher` deliberately has no pool and records nothing (see
/// `master_fetch`'s doc comment), which is what keeps every other test's
/// ledger assertions honest -- so the sweep's `purpose = 'identity'` row is
/// exercised here by a fake that repeats the seam's ONE line, using the same
/// production constant and cost function `BlpapiMasterFetcher::identity_sweep`
/// uses. What the fake stands in for is the transport, never the accounting.
struct SweepFake {
    inner: MockMasterFetcher,
    pool: sqlx::PgPool,
}

impl SweepFake {
    fn new(pool: &sqlx::PgPool, sweep_raw: serde_json::Value) -> Self {
        Self {
            inner: MockMasterFetcher { sweep_raw, ..Default::default() },
            pool: pool.clone(),
        }
    }
    fn call_count(&self) -> usize { self.inner.call_count() }
    /// Calls whose recorded name starts with `prefix` -- the sweep's own
    /// requests are `identity_sweep:*`, while the P9 lifecycle the sweep hands
    /// off to makes its own (`hist_ids:*`, `ma_deals:*`) through the same fake.
    fn calls_starting(&self, prefix: &str) -> usize {
        self.inner.calls.lock().unwrap().iter()
            .filter(|c| c.starts_with(prefix)).count()
    }
}

impl MasterFetcher for SweepFake {
    async fn identity(&self, s: &[String]) -> AppResult<Answered<Vec<IdentityBlock>>> {
        self.inner.identity(s).await
    }
    async fn hist_ids(&self, security: &str, anchor: &str, start: NaiveDate)
        -> AppResult<Vec<HistIdRow>> {
        self.inner.hist_ids(security, anchor, start).await
    }
    async fn instrument_list(&self, q: &str, yk: Option<&str>, max: u32)
        -> AppResult<Answered<Vec<Candidate>>> {
        self.inner.instrument_list(q, yk, max).await
    }
    async fn corp_actions(&self, s: &[String]) -> AppResult<Answered<CorpActionsTables>> {
        self.inner.corp_actions(s).await
    }
    async fn market_status(&self, s: &[String])
        -> AppResult<Answered<Vec<(String, String)>>> {
        self.inner.market_status(s).await
    }
    async fn ma_deals(&self, s: &str) -> AppResult<Answered<MaDealsOutcome>> {
        self.inner.ma_deals(s).await
    }
    async fn action_terms(&self, id: &str) -> AppResult<Answered<Option<ActionTerms>>> {
        self.inner.action_terms(id).await
    }
    async fn identity_sweep(&self, securities: &[String], sweep: &str)
        -> AppResult<Answered<Vec<SweepAnswer>>> {
        let answered = self.inner.identity_sweep(securities, sweep).await?;
        // Verbatim the seam's charge -- same purpose, same cost function.
        getbloomdata_lib::budget::record_purpose_hits(
            &self.pool, master_fetch::IDENTITY_PURPOSE,
            master_fetch::identity_sweep_hit_cost(securities.len(), sweep)).await?;
        Ok(answered)
    }
}

fn dt(s: &str) -> NaiveDate { s.parse().unwrap() }

/// One asset class, one view, N active book instruments with today-valid
/// `bdp_security` aliases. Returns (view_id, class_id, [(instrument_id, security)]).
async fn sweep_scaffold(pool: &sqlx::PgPool, stem: &str, sweep: &str,
                        ma_capable: bool, labels: &[&str])
    -> (i64, i64, Vec<(i64, String)>)
{
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, identity_sweep, ma_capable)
         VALUES ($1,$2,$3) RETURNING id")
        .bind(uniq(stem)).bind(sweep).bind(ma_capable)
        .fetch_one(pool).await.unwrap();
    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq(&format!("{stem}View"))).fetch_one(pool).await.unwrap();
    let mut out = Vec::new();
    for label in labels {
        let inst = store::create(pool).await.unwrap();
        let iid = inst.instrument_id;
        let sec = format!("{} Corp", uniq(&format!("{stem}{label}")));
        let mut tx = pool.begin().await.unwrap();
        store::insert_alias(&mut tx, iid, &NewAlias {
            id_type: "bdp_security".into(), value: sec.clone(), exch_code: None,
            valid_from: dt("2000-01-03"), valid_to: None, source: "user".into(),
            bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                     VALUES ($1,$2,$3)")
            .bind(iid).bind(class).bind(uniq(label)).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
            .bind(view).bind(iid).execute(pool).await.unwrap();
        out.push((iid, sec));
    }
    (view, class, out)
}

/// A `kind: "reference"` reply, the shape `parse_reference_message` reads:
/// `fields` come back in `fieldData`, `absent` arrive as `fieldExceptions`
/// (probe F9's `field_not_applicable`).
/// `(security, fields that answered, fields that came back N/A)`.
type SweepRow<'a> = (&'a str, Vec<(&'a str, &'a str)>, Vec<&'a str>);

fn sweep_reply(rows: &[SweepRow]) -> serde_json::Value {
    let secs: Vec<serde_json::Value> = rows.iter().map(|(security, fields, absent)| {
        let data: serde_json::Map<String, serde_json::Value> = fields.iter()
            .map(|(k, v)| ((*k).to_string(), serde_json::json!(v))).collect();
        let exceptions: Vec<serde_json::Value> = absent.iter().map(|f| serde_json::json!({
            "fieldId": f,
            "errorInfo": {"category": "BAD_FLD", "subcategory": "NOT_APPLICABLE_TO_REF_DATA",
                          "message": "Field not applicable to security"}})).collect();
        serde_json::json!({"security": security, "fieldExceptions": exceptions,
                           "fieldData": data})
    }).collect();
    serde_json::json!([{"securityData": secs}])
}

async fn issue_codes(pool: &sqlx::PgPool, instrument_id: i64) -> Vec<String> {
    sqlx::query_scalar("SELECT code FROM ingest_issue WHERE instrument_id = $1 ORDER BY code")
        .bind(instrument_id).fetch_all(pool).await.unwrap()
}

/// (a) A matured bond retires. MATURITY came back dated yesterday, the class
/// is `maturity`-swept and not `ma_capable`, so the sweep hands it to P9's
/// `retire_path` -- and the wire seam charged `purpose = 'identity'`.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_matured_bond_reaches_the_retire_path_and_the_identity_ledger() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpMat", "maturity", false, &["Bond"]).await;
    let (iid, sec) = members[0].clone();
    let fake = SweepFake::new(&pool, sweep_reply(&[
        (&sec, vec![("MATURITY", "2026-08-19")], vec!["CALLED_DT", "INACTIVE_DATE"])]));
    let before_id: i64 = sqlx::query_scalar("SELECT coalesce(max(id),0) FROM hit_ledger")
        .fetch_one(&pool).await.unwrap();

    let summary = identity::run_sweep(&pool, &fake, view, today).await.unwrap();

    assert_eq!(summary.swept, 1);
    assert_eq!(summary.triggered, 1, "MATURITY yesterday is a retirement trigger");
    assert_eq!(summary.anomalies, 0);
    assert_eq!(fake.calls_starting("identity_sweep:"), 1,
               "one batched ReferenceDataRequest for the class");
    assert!(issue_codes(&pool, iid).await.iter().any(|c| c == "lifecycle_retired"),
            "the sweep must route into P9's existing retire_path, not a new one");
    assert_eq!(fake.calls_starting("hist_ids:"), 1,
               "and it is genuinely P9's retire_path: the identifier-history \
                refresh that path performs is what made this call");
    assert_eq!(fake.calls_starting("ma_deals:"), 0,
               "the class is not ma_capable, so no deal list is bought");

    let (rows, hits): (i64, i64) = sqlx::query_as(
        "SELECT count(*)::bigint, coalesce(sum(estimated_hits),0)::bigint
           FROM hit_ledger WHERE id > $1 AND purpose = 'identity' AND run_id IS NULL")
        .bind(before_id).fetch_one(&pool).await.unwrap();
    assert_eq!(rows, 1, "one request, one ledger row, charged at the seam");
    assert_eq!(hits, 3, "1 security x the 3 maturity sweep fields");
}

/// (b) A class that opted out plans nothing -- and therefore reaches no wire.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_class_with_sweep_none_plans_no_request() {
    let pool = common::pool().await;
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpNone", "none", true, &["Equity"]).await;
    let (iid, sec) = members[0].clone();
    // A reply that WOULD retire it, if anything ever asked.
    let fake = SweepFake::new(&pool, sweep_reply(&[
        (&sec, vec![("MARKET_STATUS", "ACQU")], vec![])]));

    assert!(identity::plan_sweep(&pool, view).await.unwrap().is_empty());
    let summary = identity::run_sweep(&pool, &fake, view, dt("2026-08-20")).await.unwrap();
    assert_eq!(summary.batches, 0);
    assert_eq!(fake.call_count(), 0, "'none' must never reach Bloomberg");
    assert!(issue_codes(&pool, iid).await.is_empty());
}

/// (c) Probe F5's trap, guarded by construction: spot FX/metals report
/// MATURITY as the rolling T+2 SETTLEMENT date. A class created without an
/// explicit `identity_sweep` is `'none'`, so that date is never fetched and
/// never read -- no date heuristic stands between the two.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_spot_shaped_class_defaults_to_no_sweep_so_t_plus_2_never_retires_it() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("SwpSpot")).fetch_one(&pool).await.unwrap();
    let sweep: String = sqlx::query_scalar(
        "SELECT identity_sweep FROM asset_class WHERE id = $1")
        .bind(class).fetch_one(&pool).await.unwrap();
    assert_eq!(sweep, "none",
        "F5: a spot class must never be maturity-swept; the default is the guard");

    let view: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("SwpSpotView")).fetch_one(&pool).await.unwrap();
    let inst = store::create(&pool).await.unwrap();
    let sec = format!("{} Curncy", uniq("SwpSpotSec"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: sec.clone(), exch_code: None,
        valid_from: dt("2000-01-03"), valid_to: None, source: "user".into(),
        bbg_action_id: None, anchoring_identifier: None }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(inst.instrument_id).bind(class).bind(uniq("EURUSD"))
        .execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(view).bind(inst.instrument_id).execute(&pool).await.unwrap();

    // T+2 settlement, exactly what a spot pair answers -- and it is in the
    // FUTURE, so even a date heuristic would need this class never asked.
    let fake = SweepFake::new(&pool, sweep_reply(&[
        (&sec, vec![("MATURITY", "2026-08-24")], vec![])]));
    let summary = identity::run_sweep(&pool, &fake, view, today).await.unwrap();

    assert_eq!(summary.batches, 0);
    assert_eq!(fake.call_count(), 0);
    assert!(issue_codes(&pool, inst.instrument_id).await.is_empty(),
            "an FX pair two days after onboarding must still be alive");
}

/// F9: `field_not_applicable` on SOME sweep fields is normal (open-end funds
/// have no INACTIVE_DATE). The verdict is taken on whichever fields DID
/// return -- for a fund, MARKET_STATUS alone.
#[tokio::test]
#[ignore = "requires postgres"]
async fn triggers_are_evaluated_on_the_fields_that_returned() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpF9", "market_status", false, &["Dead", "Live"]).await;
    let (dead_id, dead_sec) = members[0].clone();
    let (live_id, live_sec) = members[1].clone();
    let fake = SweepFake::new(&pool, sweep_reply(&[
        // Both funds: INACTIVE_DATE is not applicable. Normal, not an anomaly.
        (&dead_sec, vec![("MARKET_STATUS", "ACQU")], vec!["INACTIVE_DATE"]),
        (&live_sec, vec![("MARKET_STATUS", "ACTV")], vec!["INACTIVE_DATE"])]));

    let summary = identity::run_sweep(&pool, &fake, view, today).await.unwrap();

    assert_eq!(summary.swept, 2);
    assert_eq!(summary.triggered, 1);
    assert_eq!(summary.anomalies, 0,
        "a per-field N/A is normal (F9), never an anomaly");
    assert!(issue_codes(&pool, dead_id).await.iter().any(|c| c == "lifecycle_retired"));
    assert!(issue_codes(&pool, live_id).await.is_empty(), "ACTV stays untouched");
}

/// F9's other half: a security where EVERY sweep field failed is an anomaly --
/// logged, advisory, and the rest of the batch is still judged.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_security_with_no_answering_field_is_an_anomaly_and_the_sweep_continues() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpF9b", "market_status", false, &["Mute", "Dead"]).await;
    let (mute_id, mute_sec) = members[0].clone();
    let (dead_id, dead_sec) = members[1].clone();
    let fake = SweepFake::new(&pool, sweep_reply(&[
        (&mute_sec, vec![], vec!["MARKET_STATUS", "INACTIVE_DATE"]),
        (&dead_sec, vec![("MARKET_STATUS", "ACQU"), ("INACTIVE_DATE", "2026-05-04")],
         vec![])]));

    let summary = identity::run_sweep(&pool, &fake, view, today).await.unwrap();

    assert_eq!(summary.anomalies, 1);
    assert_eq!(summary.triggered, 1, "one mute security must not silence the batch");
    assert!(issue_codes(&pool, mute_id).await.iter().any(|c| c == "identity_sweep_no_answer"),
            "all-fields-failed is advisory and durable, never a retirement");
    assert!(!issue_codes(&pool, mute_id).await.iter().any(|c| c == "lifecycle_retired"),
            "silence must never be read as death");
    assert!(issue_codes(&pool, dead_id).await.iter().any(|c| c == "lifecycle_retired"));
}

/// The sweep runs every week; a dead name the user has not retired yet must
/// not re-buy its investigation every week. P6's 30-day cooldown is the shared
/// brake, and the sweep arms it by recording the verbatim MARKET_STATUS the
/// same way P6's own check does.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_second_sweep_inside_the_cooldown_does_not_re_investigate() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpCool", "market_status", false, &["Dead"]).await;
    let (_iid, sec) = members[0].clone();
    let reply = sweep_reply(&[(&sec, vec![("MARKET_STATUS", "ACQU")], vec![])]);

    let first = SweepFake::new(&pool, reply.clone());
    let s1 = identity::run_sweep(&pool, &first, view, today).await.unwrap();
    assert_eq!(s1.triggered, 1);
    assert_eq!(s1.cooldown_skipped, 0);
    assert_eq!(first.calls_starting("hist_ids:"), 1, "the retire path ran");

    // Same week, or any day inside RECHECK_DAYS: still triggered, still asked
    // (the sweep is a batch, it costs nothing extra per name), but the
    // investigation behind it is not bought a second time.
    let second = SweepFake::new(&pool, reply);
    let s2 = identity::run_sweep(&pool, &second,
                                 view, today + chrono::Duration::days(7)).await.unwrap();
    assert_eq!(s2.triggered, 1);
    assert_eq!(s2.cooldown_skipped, 1);
    assert_eq!(second.calls_starting("hist_ids:"), 0,
               "a name already investigated this month is left alone");
    assert_eq!(second.calls_starting("identity_sweep:"), 1,
               "the sweep itself still asks -- the cooldown gates the dispatch, \
                not the batched question");
}

/// Review finding 2. P6 records the status of EVERY candidate it asks, ACTV
/// included, before it decides whether to investigate. If the sweep's cooldown
/// matched any recorded status, a routine "still alive" answer would suppress
/// the delisting that follows it -- a halted equity that answers ACTV on Monday
/// and is delisted on Thursday would wait a month for its investigation.
/// The cooldown is armed by a recorded DEATH, never by a recorded answer.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_recent_actv_answer_does_not_suppress_a_first_investigation() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpActv", "market_status", false, &["Halted"]).await;
    let (iid, sec) = members[0].clone();

    // Exactly what `lifecycle::run_check` writes for a candidate that answered
    // ACTV yesterday: same attr, same source, same shape.
    let mut tx = pool.begin().await.unwrap();
    store::set_attr(&mut tx, iid, getbloomdata_lib::lifecycle::MARKET_STATUS_ATTR,
                    "ACTV", today - chrono::Duration::days(1), "bloomberg", None)
        .await.unwrap();
    tx.commit().await.unwrap();

    // Today the sweep learns it is going away after all.
    let fake = SweepFake::new(&pool, sweep_reply(&[
        (&sec, vec![("MARKET_STATUS", "ACTV"), ("INACTIVE_DATE", "2026-08-28")],
         vec![])]));
    let summary = identity::run_sweep(&pool, &fake, view, today).await.unwrap();

    assert_eq!(summary.triggered, 1);
    assert_eq!(summary.cooldown_skipped, 0,
               "yesterday's ACTV is not an investigation and must not stand in for one");
    assert_eq!(fake.calls_starting("hist_ids:"), 1, "the retire path actually ran");
    assert!(issue_codes(&pool, iid).await.iter().any(|c| c == "lifecycle_retired"));
}

/// Review finding 1, end to end. Bloomberg answers a blank string where it has
/// nothing to say. Blank is silence: a live name whose fields all come back
/// empty must reach the F9 anomaly path, never a retirement.
#[tokio::test]
#[ignore = "requires postgres"]
async fn blank_field_values_are_silence_and_never_retire_a_live_instrument() {
    let pool = common::pool().await;
    let today = dt("2026-08-20");
    let (view, _class, members) =
        sweep_scaffold(&pool, "SwpBlank", "market_status", false, &["Blank", "Live"]).await;
    let (blank_id, blank_sec) = members[0].clone();
    let (live_id, live_sec) = members[1].clone();
    let fake = SweepFake::new(&pool, sweep_reply(&[
        (&blank_sec, vec![("MARKET_STATUS", ""), ("INACTIVE_DATE", "  ")], vec![]),
        (&live_sec, vec![("MARKET_STATUS", "ACTV"), ("INACTIVE_DATE", "")], vec![])]));

    let summary = identity::run_sweep(&pool, &fake, view, today).await.unwrap();

    assert_eq!(summary.triggered, 0,
               "an empty string is not a status, and a blank INACTIVE_DATE is not a date");
    assert_eq!(summary.anomalies, 1, "the all-blank security is F9's anomaly");
    assert!(issue_codes(&pool, blank_id).await.iter().any(|c| c == "identity_sweep_no_answer"));
    assert!(!issue_codes(&pool, blank_id).await.iter().any(|c| c == "lifecycle_retired"),
            "silence must never be read as death");
    assert!(issue_codes(&pool, live_id).await.is_empty(),
            "and a blank alongside a real ACTV changes nothing");
    assert_eq!(fake.calls_starting("hist_ids:"), 0, "nothing was investigated");
}
