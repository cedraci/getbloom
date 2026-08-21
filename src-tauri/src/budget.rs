use crate::error::AppResult;
use crate::fetch::{FetchAsset, FetchField};
use chrono::{Datelike, NaiveDate, Weekday};
use serde::Serialize;
use sqlx::PgPool;

pub const DEFAULT_SOFT_LIMIT: i64 = 100_000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum BudgetLevel { Ok, SoftWarn, HardConfirm }

/// Inclusive weekday count between `start` and `end` (Mon..Fri). Used both by
/// the pre-flight gate estimate here and by `fetch::dispatched_hits`, which
/// needs the exact same range semantics to be comparable to it.
pub fn weekdays_between(start: NaiveDate, end: NaiveDate) -> i64 {
    let mut d = start;
    let mut n = 0;
    while d <= end {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            n += 1;
        }
        d += chrono::Duration::days(1);
    }
    n
}

pub fn estimate_eod_hits(assets: &[FetchAsset], fields: &[FetchField]) -> i64 {
    assets.iter().map(|a|
        fields.iter().filter(|f| f.asset_class_id == a.asset_class_id).count() as i64
    ).sum()
}

pub fn estimate_backfill_hits(
    assets: &[FetchAsset], fields: &[FetchField], start: NaiveDate, end: NaiveDate,
) -> i64 {
    estimate_eod_hits(assets, fields) * weekdays_between(start, end)
}

pub fn check_level(estimated: i64, today_total: i64, soft: i64) -> BudgetLevel {
    let projected = estimated + today_total;
    if projected > soft * 2 {
        BudgetLevel::HardConfirm
    } else if projected > soft {
        BudgetLevel::SoftWarn
    } else {
        BudgetLevel::Ok
    }
}

pub async fn today_hits(pool: &PgPool) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits),0)::bigint
         FROM hit_ledger WHERE occurred_on = CURRENT_DATE")
        .fetch_one(pool).await?)
}

pub async fn record_hits(pool: &PgPool, run_id: i64, estimated: i64) -> AppResult<()> {
    sqlx::query("INSERT INTO hit_ledger (run_id, estimated_hits) VALUES ($1,$2)")
        .bind(run_id).bind(estimated).execute(pool).await?;
    Ok(())
}

/// What one instrumentListRequest is charged. Whether this request is metered
/// at all is not established (Bloomberg does not document it) -- it is
/// counted, the existing over-count-is-safe policy applied to a new call site.
pub const SEARCH_HIT_COST: i64 = 1;

/// Record a metered call that belongs to no run -- an explicit search rather
/// than a scheduled or manual EOD/backfill pull. `run_id` is nullable in the
/// schema for exactly this: `hit_ledger.purpose` distinguishes the two.
pub async fn record_purpose_hits(pool: &PgPool, purpose: &str, hits: i64) -> AppResult<()> {
    sqlx::query("INSERT INTO hit_ledger (run_id, purpose, estimated_hits)
                 VALUES (NULL, $1, $2)")
        .bind(purpose).bind(hits).execute(pool).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn fixture() -> (Vec<FetchAsset>, Vec<FetchField>) {
        let mk_a = |id, class| FetchAsset {
            instrument_id: id, asset_class_id: class, class_name: format!("C{class}"),
            label: format!("A{id}"), bdp_security: format!("S{id} Equity") };
        let mk_f = |id, class, m: &str| FetchField {
            field_id: id, asset_class_id: class,
            mnemonic: m.into(), value_kind: "numeric".into() };
        // 2 equity assets x 3 equity fields + 1 index asset x 1 index field = 7
        (vec![mk_a(1, 10), mk_a(2, 10), mk_a(3, 20)],
         vec![mk_f(1, 10, "PX_LAST"), mk_f(2, 10, "PX_BID"), mk_f(3, 10, "PX_ASK"),
              mk_f(4, 20, "PX_LAST")])
    }

    #[test]
    fn eod_estimate_is_security_times_class_fields() {
        let (a, f) = fixture();
        assert_eq!(estimate_eod_hits(&a, &f), 7);
    }

    #[test]
    fn weekday_count_inclusive() {
        // Mon 2026-08-03 .. Fri 2026-08-14 = 10 weekdays
        let s = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        assert_eq!(weekdays_between(s, e), 10);
        // weekend-only range
        let sat = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        let sun = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        assert_eq!(weekdays_between(sat, sun), 0);
    }

    #[test]
    fn backfill_estimate_scales_by_weekdays() {
        let (a, f) = fixture();
        let s = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        assert_eq!(estimate_backfill_hits(&a, &f, s, e), 70);
    }

    #[test]
    fn levels_use_soft_and_double_soft() {
        assert_eq!(check_level(1_000, 0, 100_000), BudgetLevel::Ok);
        assert_eq!(check_level(60_000, 50_000, 100_000), BudgetLevel::SoftWarn);
        assert_eq!(check_level(150_001, 50_000, 100_000), BudgetLevel::HardConfirm);
        // cumulative: today's ledger counts toward the thresholds
        assert_eq!(check_level(1, 200_000, 100_000), BudgetLevel::HardConfirm);
    }
}
