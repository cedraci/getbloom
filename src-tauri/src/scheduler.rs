use crate::error::AppResult;
use crate::orchestrator::{self, PipelineConfig, RunOutcome};
use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Weekday};
use rand::Rng;
use sqlx::PgPool;
use std::collections::HashSet;

pub fn draw_time(window_start: NaiveTime, window_end: NaiveTime,
                 rng: &mut impl Rng) -> NaiveTime {
    let start_s = window_start.signed_duration_since(
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()).num_seconds();
    let end_s = window_end.signed_duration_since(
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()).num_seconds();
    // Guard misconfigured schedule rows (start >= end) to prevent panic on empty range
    if start_s >= end_s {
        return window_start;
    }
    let s = rng.gen_range(start_s..end_s);
    NaiveTime::from_num_seconds_from_midnight_opt(s as u32, 0).unwrap()
}

pub fn is_due(now: NaiveTime, drawn_at: NaiveTime) -> bool {
    now >= drawn_at
}

pub async fn ensure_draw(pool: &PgPool, schedule_id: i64, today: NaiveDate)
    -> AppResult<NaiveTime> {
    let row: (Option<NaiveDate>, Option<NaiveTime>, NaiveTime, NaiveTime) =
        sqlx::query_as(
            "SELECT drawn_for, drawn_at, window_start, window_end
             FROM schedule WHERE id = $1")
        .bind(schedule_id).fetch_one(pool).await?;
    if let (Some(df), Some(da)) = (row.0, row.1) {
        if df == today {
            return Ok(da);  // never re-roll within a day
        }
    }
    let t = draw_time(row.2, row.3, &mut rand::thread_rng());
    sqlx::query("UPDATE schedule SET drawn_for = $2, drawn_at = $3 WHERE id = $1")
        .bind(schedule_id).bind(today).bind(t).execute(pool).await?;
    Ok(t)
}

pub async fn already_ran_today(pool: &PgPool, view_id: i64, today: NaiveDate)
    -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM run
         WHERE view_id = $1 AND kind = 'eod' AND trigger_kind = 'scheduled'
           AND status <> 'failed' AND started_at::date = $2")
        .bind(view_id).bind(today).fetch_one(pool).await?;
    Ok(n > 0)
}

// Policy (controller-ruled): a view gets at most this many FAILED scheduled attempts
// per day; beyond that, tick skips it until tomorrow's re-draw rather than retrying
// every heartbeat for the rest of the day and burning more Bloomberg hits.
const MAX_FAILED_SCHEDULED_ATTEMPTS_PER_DAY: i64 = 3;
const GIVE_UP_MSG: &str = "giving up for today after 3 failed attempts";

pub async fn failed_attempts_today(pool: &PgPool, view_id: i64, today: NaiveDate)
    -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM run
         WHERE view_id = $1 AND kind = 'eod' AND trigger_kind = 'scheduled'
           AND status = 'failed' AND started_at::date = $2")
        .bind(view_id).bind(today).fetch_one(pool).await?;
    Ok(n)
}

// EOD data exists only for trading days; a weekend run would store misleading
// weekend-dated snapshots and waste Bloomberg hits for no reason.
pub fn is_weekend(d: NaiveDate) -> bool {
    matches!(d.weekday(), Weekday::Sat | Weekday::Sun)
}

// Amendment A1: daily runs fetch the PREVIOUS trading day, never live/today values.
// Monday's run targets Friday; any other weekday targets the day before.
pub fn previous_weekday(d: NaiveDate) -> NaiveDate {
    let mut p = d - Duration::days(1);
    while matches!(p.weekday(), Weekday::Sat | Weekday::Sun) {
        p -= Duration::days(1);
    }
    p
}

pub async fn tick(pool: &PgPool, cfg: &PipelineConfig,
                  now: chrono::DateTime<chrono::Local>) -> AppResult<Vec<i64>> {
    let today = now.date_naive();
    if is_weekend(today) {
        return Ok(vec![]);
    }
    let schedules: Vec<(i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, view_id, last_result FROM schedule WHERE active")
        .fetch_all(pool).await?;
    let mut launched = Vec::new();
    for (sid, view_id, last_result) in schedules {
        // Isolate per-schedule errors: one schedule's failure never blocks the others
        let drawn = match ensure_draw(pool, sid, today).await {
            Ok(t) => t,
            Err(e) => {
                let msg = format!("draw error: {e}");
                let _ = sqlx::query("UPDATE schedule SET last_result = $2 WHERE id = $1")
                    .bind(sid).bind(&msg).execute(pool).await;
                continue;
            }
        };

        let already_ran = match already_ran_today(pool, view_id, today).await {
            Ok(b) => b,
            Err(e) => {
                let msg = format!("check error: {e}");
                let _ = sqlx::query("UPDATE schedule SET last_result = $2 WHERE id = $1")
                    .bind(sid).bind(&msg).execute(pool).await;
                continue;
            }
        };

        if !is_due(now.time(), drawn) || already_ran {
            continue;
        }

        // Cap retries: after MAX_FAILED_SCHEDULED_ATTEMPTS_PER_DAY failures today,
        // skip this view until tomorrow instead of retrying every heartbeat.
        let failed_count = match failed_attempts_today(pool, view_id, today).await {
            Ok(n) => n,
            Err(e) => {
                let msg = format!("check error: {e}");
                let _ = sqlx::query("UPDATE schedule SET last_result = $2 WHERE id = $1")
                    .bind(sid).bind(&msg).execute(pool).await;
                continue;
            }
        };
        if failed_count >= MAX_FAILED_SCHEDULED_ATTEMPTS_PER_DAY {
            if last_result.as_deref() != Some(GIVE_UP_MSG) {
                let _ = sqlx::query("UPDATE schedule SET last_result = $2 WHERE id = $1")
                    .bind(sid).bind(GIVE_UP_MSG).execute(pool).await;
            }
            continue;
        }

        // Amendment A1: the daily run targets the previous trading day's close, never
        // today's live values. The weekend guard above stays: Monday's run covers Friday.
        let obs_date = previous_weekday(today);
        let result = orchestrator::run_eod(pool, cfg, view_id, "scheduled", obs_date, false).await;
        let msg = match &result {
            Ok(RunOutcome::Completed { run_id, summary }) =>
                format!("ok run={run_id} upserted={} issues={}",
                        summary.upserted, summary.issues),
            Ok(RunOutcome::NeedsConfirmation { estimated, .. }) =>
                format!("blocked: needs confirmation for {estimated} estimated hits"),
            Err(e) => format!("failed: {e}"),
        };
        let _ = sqlx::query("UPDATE schedule SET last_result = $2 WHERE id = $1")
            .bind(sid).bind(&msg).execute(pool).await;
        if matches!(result, Ok(RunOutcome::Completed { .. })) {
            launched.push(view_id);
        }
    }
    Ok(launched)
}

pub fn missing_weekdays(present: &HashSet<NaiveDate>,
                        start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) && !present.contains(&d) {
            out.push(d);
        }
        d += Duration::days(1);
    }
    out
}

fn next_weekday(d: NaiveDate) -> NaiveDate {
    let mut n = d + Duration::days(1);
    while matches!(n.weekday(), Weekday::Sat | Weekday::Sun) {
        n += Duration::days(1);
    }
    n
}

pub fn group_ranges(dates: &[NaiveDate], cap_days: i64) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out: Vec<(NaiveDate, NaiveDate)> = Vec::new();
    for &d in dates {
        match out.last_mut() {
            Some((s, e)) if next_weekday(*e) == d && (d - *s).num_days() < cap_days =>
                *e = d,
            _ => out.push((d, d)),
        }
    }
    out
}

pub async fn detect_gaps(pool: &PgPool, view_id: i64, lookback_days: i64,
                         today: NaiveDate) -> AppResult<Vec<(NaiveDate, NaiveDate)>> {
    let start = today - Duration::days(lookback_days);
    let end = today - Duration::days(1);
    let rows: Vec<(NaiveDate,)> = sqlx::query_as(
        "SELECT DISTINCT o.obs_date FROM observation o
         JOIN view_asset va ON va.asset_id = o.asset_id
         WHERE va.view_id = $1 AND o.obs_date BETWEEN $2 AND $3")
        .bind(view_id).bind(start).bind(end).fetch_all(pool).await?;
    let present: HashSet<NaiveDate> = rows.into_iter().map(|r| r.0).collect();
    Ok(group_ranges(&missing_weekdays(&present, start, end),
                    orchestrator::BACKFILL_CAP_DAYS))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashSet;

    #[test]
    fn draw_stays_inside_window_and_varies() {
        let s = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let e = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let mut rng = StdRng::seed_from_u64(42);
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let t = draw_time(s, e, &mut rng);
            assert!(t >= s && t < e, "drew {t} outside window");
            seen.insert(t);
        }
        assert!(seen.len() > 150, "draws should vary, got {} distinct", seen.len());
    }

    #[test]
    fn draw_time_guards_misconfigured_bounds() {
        let t = NaiveTime::from_hms_opt(14, 30, 0).unwrap();
        let mut rng = StdRng::seed_from_u64(99);
        // Equal bounds should not panic and should return start time
        assert_eq!(draw_time(t, t, &mut rng), t);
        // Inverted bounds should not panic and should return start time
        assert_eq!(draw_time(t, NaiveTime::from_hms_opt(9, 0, 0).unwrap(), &mut rng), t);
    }

    #[test]
    fn due_logic_covers_catchup() {
        let drawn = NaiveTime::from_hms_opt(11, 30, 0).unwrap();
        assert!(!is_due(NaiveTime::from_hms_opt(9, 0, 0).unwrap(), drawn));
        assert!(is_due(NaiveTime::from_hms_opt(11, 30, 0).unwrap(), drawn));
        assert!(is_due(NaiveTime::from_hms_opt(17, 59, 0).unwrap(), drawn)); // late launch
    }

    #[test]
    fn is_weekend_flags_sat_and_sun_only() {
        assert!(is_weekend(NaiveDate::from_ymd_opt(2026, 8, 15).unwrap())); // Sat
        assert!(is_weekend(NaiveDate::from_ymd_opt(2026, 8, 16).unwrap())); // Sun
        assert!(!is_weekend(NaiveDate::from_ymd_opt(2026, 8, 14).unwrap())); // Fri
        assert!(!is_weekend(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap())); // Mon
    }

    #[test]
    fn previous_weekday_mid_week_is_prior_day() {
        // Tuesday 2026-08-18 -> Monday 2026-08-17
        assert_eq!(previous_weekday(NaiveDate::from_ymd_opt(2026, 8, 18).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 17).unwrap());
        // Friday 2026-08-14 -> Thursday 2026-08-13
        assert_eq!(previous_weekday(NaiveDate::from_ymd_opt(2026, 8, 14).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 13).unwrap());
    }

    #[test]
    fn previous_weekday_monday_is_prior_friday() {
        assert_eq!(previous_weekday(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 14).unwrap());
    }

    #[test]
    fn previous_weekday_saturday_is_friday() {
        assert_eq!(previous_weekday(NaiveDate::from_ymd_opt(2026, 8, 15).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 14).unwrap());
    }

    #[test]
    fn previous_weekday_sunday_is_friday() {
        assert_eq!(previous_weekday(NaiveDate::from_ymd_opt(2026, 8, 16).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 14).unwrap());
    }

    #[test]
    fn missing_weekdays_ignores_weekends() {
        let mut present = HashSet::new();
        present.insert(NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()); // Mon
        present.insert(NaiveDate::from_ymd_opt(2026, 8, 12).unwrap()); // Wed
        let start = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();  // Sat
        let end = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();   // Wed
        let missing = missing_weekdays(&present, start, end);
        assert_eq!(missing, vec![NaiveDate::from_ymd_opt(2026, 8, 11).unwrap()]); // Tue only
    }

    #[test]
    fn ranges_group_contiguous_weekdays_and_respect_cap() {
        let d = |m: u32, day: u32| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        // Thu 8/6, Fri 8/7, Mon 8/10 are weekday-contiguous; Wed 8/19 is separate
        let ranges = group_ranges(&[d(8,6), d(8,7), d(8,10), d(8,19)], 30);
        assert_eq!(ranges, vec![(d(8,6), d(8,10)), (d(8,19), d(8,19))]);
        // cap splits long runs
        let long: Vec<_> = (0..40)
            .map(|i| d(6, 1) + chrono::Duration::days(i))
            .filter(|x| !matches!(x.weekday(),
                chrono::Weekday::Sat | chrono::Weekday::Sun))
            .collect();
        for (s, e) in group_ranges(&long, 30) {
            assert!((e - s).num_days() < 30);
        }
    }
}
