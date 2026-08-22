//! P11 cadence + fetch-capability + identity-sweep schema (migration 0014).
//! See docs/superpowers/specs/2026-08-22-p11-cadence-and-fetch-capability-design.md
//! sections 11.1, 11.2, 11.8.
mod common;

use common::uniq;
use getbloomdata_lib::{fields, registry, views};

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
