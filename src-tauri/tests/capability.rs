//! P9 capability flags: per-asset-class behaviour switches. See
//! docs/superpowers/specs/2026-08-21-p9-p10-multi-asset-and-production-ops-design.md.
mod common;

use common::uniq;

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
