use crate::error::AppResult;
use crate::orchestrator::{self, GapBackfillOutcome, PipelineConfig, RunOutcome};
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

// A scheduled verify IS that day's scheduled run: without widening this past
// 'eod', a completed verify would not stop the EOD run from firing an hour
// later and double-charging the day.
pub async fn already_ran_today(pool: &PgPool, view_id: i64, today: NaiveDate)
    -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM run
         WHERE view_id = $1 AND kind IN ('eod','verify') AND trigger_kind = 'scheduled'
           AND status <> 'failed' AND started_at::date = $2")
        .bind(view_id).bind(today).fetch_one(pool).await?;
    Ok(n > 0)
}

// Policy (controller-ruled): a view gets at most this many FAILED scheduled attempts
// per day; beyond that, tick skips it until tomorrow's re-draw rather than retrying
// every heartbeat for the rest of the day and burning more Bloomberg hits.
const MAX_FAILED_SCHEDULED_ATTEMPTS_PER_DAY: i64 = 3;
const GIVE_UP_MSG: &str = "giving up for today after 3 failed attempts";

// Same widening as already_ran_today: three failed verify attempts must also
// stop the day, not just three failed EOD attempts.
pub async fn failed_attempts_today(pool: &PgPool, view_id: i64, today: NaiveDate)
    -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM run
         WHERE view_id = $1 AND kind IN ('eod','verify') AND trigger_kind = 'scheduled'
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

/// ISO weekday, matching schedule.verify_dow: 1 = Monday .. 7 = Sunday.
pub fn iso_dow(d: NaiveDate) -> i16 {
    d.weekday().number_from_monday() as i16
}

/// The verify run covers the trailing five weekdays: `end` plus four more
/// weekdays back. One week of history is enough to catch the common
/// restatement (yesterday's close corrected today) without pricing a
/// five-fold budget surprise into every single day.
pub fn verify_window_start(end: NaiveDate) -> NaiveDate {
    let mut d = end;
    for _ in 0..4 {
        d = previous_weekday(d);
    }
    d
}

/// Schedules eligible to fire. A schedule is due only when BOTH it and its view
/// are active -- retiring a view has to stop its scheduled runs, or "retire"
/// would mean nothing for the one entity that drives collection.
pub async fn due_schedules(pool: &PgPool)
    -> AppResult<Vec<(i64, i64, Option<String>, Option<i16>, Option<NaiveDate>)>> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.view_id, s.last_result, s.verify_dow, s.last_verified_on
         FROM schedule s JOIN view v ON v.id = s.view_id
         WHERE s.active AND v.active")
        .fetch_all(pool).await?)
}

pub async fn tick(pool: &PgPool, cfg: &PipelineConfig,
                  now: chrono::DateTime<chrono::Local>) -> AppResult<Vec<i64>> {
    let today = now.date_naive();
    if is_weekend(today) {
        return Ok(vec![]);
    }
    let schedules = due_schedules(pool).await?;
    let mut launched = Vec::new();
    for (sid, view_id, last_result, verify_dow, last_verified_on) in schedules {
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

        // Downtime recovery comes first: fill the weekdays this view missed
        // while the machine was off, BEFORE the day's own run. It is
        // budget-gated and capped at one attempt per day inside
        // `run_gap_backfill`, and it is a backfill, so it never stands in for
        // the EOD run that follows. Per-schedule isolation as everywhere else
        // here: a recovery failure is reported and the day proceeds.
        let mut note = String::new();
        match orchestrator::run_gap_backfill(pool, cfg, view_id, today).await {
            Ok(GapBackfillOutcome::Ran { days, .. }) =>
                note = format!("gap backfill: {days} days; "),
            Ok(GapBackfillOutcome::NeedsConfirmation { estimated, .. }) =>
                note = format!("gaps need confirmation ({estimated} est. hits); "),
            // Nothing to say: no gaps, or today's one attempt is already spent.
            Ok(GapBackfillOutcome::Nothing | GapBackfillOutcome::AlreadyAttemptedToday) => {}
            Err(e) => note = format!("gap backfill failed: {e}; "),
        }

        // Amendment A1 stands: the run targets the previous trading day.
        // On the schedule's verify day, the same slot instead re-reads the
        // trailing five weekdays (kind backfill, trigger scheduled) so an
        // upstream restatement is actually seen. Budget-blocked verifies
        // degrade to the normal one-day run rather than blocking the day.
        let obs_date = previous_weekday(today);
        let want_verify = verify_dow == Some(iso_dow(today))
            && last_verified_on.is_none_or(|d| d < today);
        let result = if want_verify {
            match orchestrator::run_verify(pool, cfg, view_id,
                                           verify_window_start(obs_date), obs_date).await {
                Ok(RunOutcome::NeedsConfirmation { estimated, .. }) => {
                    // Appended, never assigned: the gap-backfill outcome above
                    // is part of the same day's report.
                    note += &format!("verify skipped ({estimated} est. hits needs \
                                      confirmation); ");
                    orchestrator::run_eod(pool, cfg, view_id, "scheduled",
                                          obs_date, false).await
                }
                other => {
                    if matches!(other, Ok(RunOutcome::Completed { .. })) {
                        note += "verify ";
                        let _ = sqlx::query(
                            "UPDATE schedule SET last_verified_on = $2 WHERE id = $1")
                            .bind(sid).bind(today).execute(pool).await;
                    }
                    other
                }
            }
        } else {
            orchestrator::run_eod(pool, cfg, view_id, "scheduled", obs_date, false).await
        };
        let msg = match &result {
            Ok(RunOutcome::Completed { run_id, summary, corp_actions,
                                       quality_findings }) => {
                let ca = match corp_actions {
                    Some(c) => format!(" ca_new={} ca_amended={}", c.inserted, c.amended),
                    None => String::new(),
                };
                let q = if *quality_findings > 0 {
                    format!(" quality={quality_findings}")
                } else { String::new() };
                format!("{note}ok run={run_id} inserted={} superseded={} issues={}{q}{ca}",
                        summary.inserted, summary.superseded, summary.issues)
            }
            Ok(RunOutcome::NeedsConfirmation { estimated, .. }) =>
                format!("{note}blocked: needs confirmation for {estimated} estimated hits"),
            Err(e) => format!("{note}failed: {e}"),
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

/// How far back the scheduler's automatic downtime recovery looks, in
/// CALENDAR days (`detect_gaps` counts them that way): ten, so a machine off
/// for a long weekend or a week's holiday recovers by itself, while a view
/// left off for a month does not silently turn into a large unattended bill.
/// The manual gap report keeps its own 30 -- a human reading the list is not
/// the same as a scheduler acting on it.
pub const GAP_LOOKBACK_DAYS: i64 = 10;

/// A stretch of weekdays for which ONE instrument in a view has no
/// observation. Per-instrument, deliberately: see `detect_gaps`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Gap {
    pub instrument_id: i64,
    pub label: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
}

/// Which weekdays each member of a view is missing, over the lookback window.
///
/// Per `(instrument_id, obs_date)`, not `DISTINCT obs_date` across the whole
/// view. The old query asked "did ANY instrument report on this date", so a
/// single healthy member marked every date covered for all of them: an
/// instrument that failed for a week, or was added mid-history, produced no
/// gap and therefore no backfill. That is precisely the silent hole in a time
/// series the spec promises cannot happen, arrived at through the one function
/// whose job is to find them.
///
/// Members come from `views::view_instruments`, so a retired book entry and an
/// instrument with a pending review are both excluded -- neither should be
/// reported as a gap, because neither is supposed to be collecting.
pub async fn detect_gaps(pool: &PgPool, view_id: i64, lookback_days: i64,
                         today: NaiveDate) -> AppResult<Vec<Gap>> {
    let start = today - Duration::days(lookback_days);
    let end = today - Duration::days(1);
    let members = crate::views::view_instruments(pool, view_id).await?;
    if members.is_empty() {
        return Ok(Vec::new());
    }
    // A date is covered only when EVERY non-text field the view configures
    // for the member's class has a current raw EOD row. Text fields are
    // excluded: backfill cannot recover them by design (plan_requests), and
    // an unfixable gap is noise that buries the fixable ones.
    let fields = crate::views::view_fields(pool, view_id).await?;
    let mut expected: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for f in fields.iter().filter(|f| f.value_kind != "text") {
        *expected.entry(f.asset_class_id).or_insert(0) += 1;
    }

    let rows: Vec<(i64, NaiveDate, i64)> = sqlx::query_as(
        "SELECT o.instrument_id, o.obs_date, count(DISTINCT o.field_id)::bigint
           FROM observation o
           JOIN view_instrument vi ON vi.instrument_id = o.instrument_id
                                  AND vi.view_id = $1
           JOIN view_field vf ON vf.view_id = vi.view_id AND vf.field_id = o.field_id
           JOIN field_def fd ON fd.id = o.field_id AND fd.value_kind <> 'text'
          WHERE o.obs_date BETWEEN $2 AND $3
            AND o.system_to = 'infinity'
            AND o.layer = 'raw' AND o.granularity = 'eod'
          GROUP BY o.instrument_id, o.obs_date")
        .bind(view_id).bind(start).bind(end).fetch_all(pool).await?;

    // Days Bloomberg itself declared sessionless (holiday, suspension) are
    // covered by definition: there is nothing a backfill could fetch.
    let non_trading: Vec<(i64, NaiveDate)> = sqlx::query_as(
        "SELECT n.instrument_id, n.obs_date
           FROM non_trading_day n
           JOIN view_instrument vi ON vi.instrument_id = n.instrument_id
          WHERE vi.view_id = $1 AND n.obs_date BETWEEN $2 AND $3")
        .bind(view_id).bind(start).bind(end).fetch_all(pool).await?;

    let mut out = Vec::new();
    for m in members {
        let Some(&need) = expected.get(&m.asset_class_id) else {
            // The view fetches nothing history-shaped for this class, so no
            // date can be missing anything backfill could supply.
            continue;
        };
        let mut present: HashSet<NaiveDate> = rows.iter()
            .filter(|(iid, _, have)| *iid == m.instrument_id && *have >= need)
            .map(|(_, d, _)| *d)
            .collect();
        present.extend(non_trading.iter()
            .filter(|(iid, _)| *iid == m.instrument_id)
            .map(|(_, d)| *d));
        for (s, e) in group_ranges(&missing_weekdays(&present, start, end),
                                   orchestrator::BACKFILL_CAP_DAYS) {
            out.push(Gap { instrument_id: m.instrument_id, label: m.label.clone(),
                           start: s, end: e });
        }
    }
    Ok(out)
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
    fn verify_window_is_five_weekdays_ending_at_end() {
        // Friday 2026-08-14 back to Monday 2026-08-10
        assert_eq!(verify_window_start(NaiveDate::from_ymd_opt(2026, 8, 14).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 10).unwrap());
        // Monday 2026-08-17 back across the weekend to Tuesday 2026-08-11
        assert_eq!(verify_window_start(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 11).unwrap());
    }

    #[test]
    fn iso_dow_is_monday_one_sunday_seven() {
        assert_eq!(iso_dow(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()), 1); // Mon
        assert_eq!(iso_dow(NaiveDate::from_ymd_opt(2026, 8, 21).unwrap()), 5); // Fri
        assert_eq!(iso_dow(NaiveDate::from_ymd_opt(2026, 8, 16).unwrap()), 7); // Sun
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
