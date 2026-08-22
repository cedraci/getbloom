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

/// Is `d` the last calendar day of its month?
fn is_month_end(d: NaiveDate) -> bool {
    d.succ_opt().map(|next| next.month() != d.month()).unwrap_or(true)
}

/// Inclusive count of *period ends* in `[start, end]` for a cadence -- how many
/// prints a ranged periodic HistoricalDataRequest can return (probe F3: one row
/// per ended period, and only after the period ends).
///
/// Period ends are the calendar's, not the exchange's: Fridays for `weekly`,
/// the last calendar day of the month for `monthly`, the last day of
/// March/June/September/December for `quarterly`. The print itself is dated the
/// period's last *trading* day, which is on or before the calendar end and
/// therefore inside the same range -- counting calendar ends neither
/// double-counts nor drops a period.
///
/// Cadences with no period structure (`daily`, `irregular`) fall back to
/// `weekdays_between`, so a caller that hands this an unstructured cadence
/// charges exactly what it always charged rather than silently under-counting.
/// The match is case-insensitive: the database says `monthly`, the sidecar wire
/// says `MONTHLY`, and `dispatched_hits` passes the wire spelling straight in.
pub fn periods_between(start: NaiveDate, end: NaiveDate, cadence: &str) -> i64 {
    let is_period_end: fn(NaiveDate) -> bool = match cadence.to_ascii_lowercase().as_str() {
        "weekly" => |d: NaiveDate| d.weekday() == Weekday::Fri,
        "monthly" => is_month_end,
        "quarterly" => |d: NaiveDate| is_month_end(d) && d.month().is_multiple_of(3),
        _ => return weekdays_between(start, end),
    };
    let mut d = start;
    let mut n = 0;
    while d <= end {
        if is_period_end(d) {
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

/// P11 11.4: what ONE day of the **daily partition** costs -- every field a
/// normal run actually requests, and nothing else. Periodic history fields are
/// excluded because no run's own plan contains them (they ride the due-logic's
/// legs, priced by `estimate_periodic_hits`); pricing them per weekday would
/// have charged a monthly NAV ~21 phantom hits a day forever.
///
/// For a view with no periodic fields -- which is every view under migration
/// 0014's defaults -- this is `estimate_eod_hits` to the digit.
pub fn estimate_daily_hits(assets: &[FetchAsset], fields: &[FetchField]) -> i64 {
    let daily: Vec<&FetchField> = fields.iter()
        .filter(|f| !crate::fetch::is_periodic_history(f)).collect();
    assets.iter().map(|a|
        daily.iter().filter(|f| f.asset_class_id == a.asset_class_id).count() as i64
    ).sum()
}

/// The ranged twin of `estimate_daily_hits`.
pub fn estimate_daily_backfill_hits(
    assets: &[FetchAsset], fields: &[FetchField], start: NaiveDate, end: NaiveDate,
) -> i64 {
    estimate_daily_hits(assets, fields) * weekdays_between(start, end)
}

/// What the run's periodic legs cost: securities x that class's leg fields x
/// the number of period ends inside the leg's OWN range (F3: one row per ended
/// period). This is the ~90%-fewer-hits claim in arithmetic.
pub fn estimate_periodic_hits(
    assets: &[FetchAsset], fields: &[FetchField], legs: &[crate::fetch::PeriodicLeg],
) -> i64 {
    legs.iter().map(|leg| {
        let periods = periods_between(leg.start, leg.end, &leg.cadence);
        let pairs: i64 = assets.iter()
            .filter(|a| leg.instrument_ids.contains(&a.instrument_id))
            .map(|a| fields.iter()
                .filter(|f| f.asset_class_id == a.asset_class_id
                         && leg.field_ids.contains(&f.field_id))
                .count() as i64)
            .sum();
        pairs * periods
    }).sum()
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
        let mk_f = |id, class, m: &str| FetchField::daily_history(id, class, m, "numeric");
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

    /// P11 11.4: a periodic range is charged per *period end* inside it, not
    /// per weekday. Fridays for weekly, last calendar day of the month for
    /// monthly, last day of Mar/Jun/Sep/Dec for quarterly.
    #[test]
    fn periods_between_counts_period_ends_inclusive() {
        let (jun1, aug31) = (NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
                             NaiveDate::from_ymd_opt(2026, 8, 31).unwrap());
        assert_eq!(periods_between(jun1, aug31, "monthly"), 3, "Jun/Jul/Aug ends");
        assert_eq!(periods_between(jun1, aug31, "quarterly"), 1, "only Jun 30 is a quarter end");

        // Fridays in August 2026: 7, 14, 21, 28.
        let (aug1, aug31b) = (NaiveDate::from_ymd_opt(2026, 8, 1).unwrap(), aug31);
        assert_eq!(periods_between(aug1, aug31b, "weekly"), 4);

        // A range that ends the day before a month end contains none of it.
        assert_eq!(periods_between(NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
                                   NaiveDate::from_ymd_opt(2026, 6, 29).unwrap(),
                                   "monthly"), 0);
        // A year of quarter ends, and the leap-February edge.
        assert_eq!(periods_between(NaiveDate::from_ymd_opt(2024, 1, 1).unwrap(),
                                   NaiveDate::from_ymd_opt(2024, 12, 31).unwrap(),
                                   "quarterly"), 4);
        assert_eq!(periods_between(NaiveDate::from_ymd_opt(2024, 2, 1).unwrap(),
                                   NaiveDate::from_ymd_opt(2024, 2, 29).unwrap(),
                                   "monthly"), 1);
    }

    /// Cadences with no period structure keep the weekday count the budget has
    /// always charged -- `periods_between` is a superset of `weekdays_between`,
    /// so no caller can accidentally under-charge a daily range.
    #[test]
    fn periods_between_falls_back_to_weekdays_for_structureless_cadences() {
        let s = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        assert_eq!(periods_between(s, e, "daily"), weekdays_between(s, e));
        assert_eq!(periods_between(s, e, "irregular"), weekdays_between(s, e));
    }

    /// The sidecar speaks `MONTHLY`; the database speaks `monthly`.
    /// `dispatched_hits` hands the wire spelling straight through, so the
    /// match must not care which arrives.
    #[test]
    fn periods_between_accepts_the_sidecar_spelling() {
        let s = NaiveDate::from_ymd_opt(2026, 6, 1).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 31).unwrap();
        assert_eq!(periods_between(s, e, "MONTHLY"), 3);
        assert_eq!(periods_between(s, e, "WEEKLY"), periods_between(s, e, "weekly"));
        assert_eq!(periods_between(s, e, "QUARTERLY"), 1);
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
