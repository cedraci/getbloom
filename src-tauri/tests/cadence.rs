//! P11 cadence + fetch-capability + identity-sweep schema (migration 0014).
//! See docs/superpowers/specs/2026-08-22-p11-cadence-and-fetch-capability-design.md
//! sections 11.1, 11.2, 11.8.
mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::error::AppResult;
use getbloomdata_lib::fetch::{FetchOutcome, FetchRequest, RequestSpec};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::orchestrator::{self, DataFetcher, PipelineConfig, RunOutcome};
use getbloomdata_lib::{fetch, fields, registry, scheduler, views};
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
