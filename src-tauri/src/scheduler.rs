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

/// Is a weekly slot due today? Both weekly slots -- `verify_dow`/
/// `last_verified_on` and P11's `identity_dow`/`last_identity_on` -- ask
/// exactly this question, so they ask it through one function and cannot
/// drift: the configured ISO weekday, and not already done today.
///
/// `None` for the weekday is off, which is how `identity_dow` ships.
pub fn weekly_slot_due(dow: Option<i16>, last_on: Option<NaiveDate>,
                       today: NaiveDate) -> bool {
    dow == Some(iso_dow(today)) && last_on.is_none_or(|d| d < today)
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

// ------------------------------------------------------- P11 11.4/11.5/11.7:
// period arithmetic. Pure, so the whole cadence model is testable without a
// database, and shared by the due-logic, the gap detector and the verify
// window -- three callers that MUST agree on where a period starts and ends.

/// How many completed periods the cadence machinery looks back: two.
/// `GAP_LOOKBACK_DAYS` is meaningless at monthly scale (ten days does not
/// reach one period), and an unbounded lookback would re-buy a fund's entire
/// history the first time a print was late.
pub const PERIOD_LOOKBACK: usize = 2;

fn last_day_of_month(year: i32, month: u32) -> NaiveDate {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.pred_opt())
        .expect("a month always has a last day")
}

/// The `[start, end]` of the period of `cadence` that CONTAINS `d`, or `None`
/// for the cadences with no period structure (`daily`, `irregular`).
///
/// The ends match `budget::periods_between` exactly -- Friday for weekly, the
/// last calendar day of the month for monthly, of Mar/Jun/Sep/Dec for
/// quarterly -- because the same range is both planned here and priced there.
/// Bloomberg dates the print on the period's last *trading* day, which is on
/// or before the calendar end and therefore inside these bounds (probe F3).
pub fn period_bounds(d: NaiveDate, cadence: &str) -> Option<(NaiveDate, NaiveDate)> {
    match cadence.to_ascii_lowercase().as_str() {
        "weekly" => {
            let monday = d - Duration::days(d.weekday().num_days_from_monday() as i64);
            Some((monday, monday + Duration::days(4)))
        }
        "monthly" => Some((
            NaiveDate::from_ymd_opt(d.year(), d.month(), 1)?,
            last_day_of_month(d.year(), d.month()),
        )),
        "quarterly" => {
            let first_month = ((d.month() - 1) / 3) * 3 + 1;
            Some((
                NaiveDate::from_ymd_opt(d.year(), first_month, 1)?,
                last_day_of_month(d.year(), first_month + 2),
            ))
        }
        _ => None,
    }
}

/// The period immediately before the one starting at `start`.
pub fn previous_period(start: NaiveDate, cadence: &str) -> Option<(NaiveDate, NaiveDate)> {
    period_bounds(start - Duration::days(1), cadence)
}

/// The last `n` periods of `cadence` that have ENDED as of `today`, newest
/// first. A period whose end is today is NOT yet ended: probe F3 says the
/// print appears only after the period closes, so asking for it buys a row
/// that does not exist.
pub fn completed_periods(today: NaiveDate, cadence: &str, n: usize)
    -> Vec<(NaiveDate, NaiveDate)> {
    let Some((mut start, mut end)) = period_bounds(today, cadence) else {
        return Vec::new();
    };
    if end >= today {
        match previous_period(start, cadence) {
            Some(p) => (start, end) = p,
            None => return Vec::new(),
        }
    }
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        out.push((start, end));
        match previous_period(start, cadence) {
            Some(p) => (start, end) = p,
            None => break,
        }
    }
    out
}

/// How a period is named to a human: `2026-07`, `2026-Q3`, `2026-W31`.
pub fn period_label(start: NaiveDate, cadence: &str) -> String {
    match cadence.to_ascii_lowercase().as_str() {
        "weekly" => format!("{}-W{:02}", start.iso_week().year(), start.iso_week().week()),
        "quarterly" => format!("{}-Q{}", start.year(), (start.month() - 1) / 3 + 1),
        _ => format!("{}-{:02}", start.year(), start.month()),
    }
}

/// Is a period's missing print *anomalous* yet? Grace is calendar days after
/// the period ends (`asset_class.cadence_grace_days`), and it gates FINDINGS
/// only: for fetching, a period is askable the moment it ends.
pub fn period_overdue(period_end: NaiveDate, grace_days: i64, today: NaiveDate) -> bool {
    today > period_end + Duration::days(grace_days)
}

/// One (instrument, field, period) that should have a print and does not.
///
/// The single detection primitive behind three consumers: the due-logic
/// (which fetches every miss, overdue or not), period-shaped gaps (the overdue
/// ones), and P11 11.6's `publication_overdue` quality finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeriodMiss {
    pub instrument_id: i64,
    /// The instrument's book label, so a report can name it.
    pub label: String,
    pub asset_class_id: i64,
    pub field_id: i64,
    pub mnemonic: String,
    /// Effective cadence, database spelling.
    pub cadence: String,
    pub start: NaiveDate,
    pub end: NaiveDate,
    /// `2026-07` and friends -- what the UI shows instead of a fake day range.
    pub period: String,
    /// `today > end + asset_class.cadence_grace_days`.
    pub overdue: bool,
}

/// Every completed period, inside the lookback, for which a view's periodic x
/// history pairs have no current observation.
///
/// Members and fields come from `views::view_instruments` / `views::view_fields`,
/// so a retired book entry, a pending resolution and an inactive field are all
/// excluded here for the same reasons `detect_gaps` excludes them.
pub async fn missing_periods(pool: &PgPool, view_id: i64, today: NaiveDate,
                             lookback: usize) -> AppResult<Vec<PeriodMiss>> {
    let members = crate::views::view_instruments(pool, view_id).await?;
    if members.is_empty() {
        return Ok(Vec::new());
    }
    let fields = crate::views::view_fields(pool, view_id).await?;
    // The periodic x history partition, spelled with the SAME predicate the
    // planner uses -- `fetch::is_periodic_history` -- so "planned by the
    // due-logic" and "gap-detected as a period" can never disagree.
    let periodic: Vec<(&crate::fields::FieldDef, &str)> = fields.iter()
        .filter(|vf| crate::fetch::is_periodic_history_parts(
            &vf.def.value_kind, &vf.def.fetch_via, &vf.effective_cadence))
        .map(|vf| (&vf.def, vf.effective_cadence.as_str()))
        .collect();
    if periodic.is_empty() {
        return Ok(Vec::new());
    }

    let grace: Vec<(i64, i32)> =
        sqlx::query_as("SELECT id, cadence_grace_days FROM asset_class")
            .fetch_all(pool).await?;
    let grace_of = |class_id: i64| grace.iter()
        .find(|(id, _)| *id == class_id).map(|&(_, g)| g as i64).unwrap_or(0);

    // Everything the lookback could possibly need, in one query.
    let earliest = periodic.iter()
        .filter_map(|(_, cad)| completed_periods(today, cad, lookback).last().map(|p| p.0))
        .min();
    let Some(earliest) = earliest else {
        return Ok(Vec::new());
    };
    let instrument_ids: Vec<i64> = members.iter().map(|m| m.instrument_id).collect();
    let field_ids: Vec<i64> = periodic.iter().map(|(f, _)| f.id).collect();
    let prints: Vec<(i64, i64, NaiveDate)> = sqlx::query_as(
        "SELECT instrument_id, field_id, obs_date FROM observation
          WHERE instrument_id = ANY($1) AND field_id = ANY($2)
            AND obs_date BETWEEN $3 AND $4
            AND system_to = 'infinity'
            AND layer = 'raw' AND granularity = 'eod'")
        .bind(&instrument_ids).bind(&field_ids).bind(earliest).bind(today)
        .fetch_all(pool).await?;

    let mut out = Vec::new();
    for m in &members {
        for (def, cadence) in &periodic {
            if def.asset_class_id != m.asset_class_id {
                continue;
            }
            for (start, end) in completed_periods(today, cadence, lookback) {
                let printed = prints.iter().any(|&(iid, fid, on)|
                    iid == m.instrument_id && fid == def.id && on >= start && on <= end);
                if printed {
                    continue;
                }
                out.push(PeriodMiss {
                    instrument_id: m.instrument_id,
                    label: m.label.clone(),
                    asset_class_id: m.asset_class_id,
                    field_id: def.id,
                    mnemonic: def.mnemonic.clone(),
                    cadence: (*cadence).to_string(),
                    start,
                    end,
                    period: period_label(start, cadence),
                    overdue: period_overdue(end, grace_of(m.asset_class_id), today),
                });
            }
        }
    }
    Ok(out)
}

/// Schedules eligible to fire. A schedule is due only when BOTH it and its view
/// are active -- retiring a view has to stop its scheduled runs, or "retire"
/// would mean nothing for the one entity that drives collection.
pub type DueSchedule = (i64, i64, Option<String>, Option<i16>, Option<NaiveDate>,
                        Option<i16>, Option<NaiveDate>);

pub async fn due_schedules(pool: &PgPool) -> AppResult<Vec<DueSchedule>> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.view_id, s.last_result, s.verify_dow, s.last_verified_on,
                s.identity_dow, s.last_identity_on
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
    for (sid, view_id, last_result, verify_dow, last_verified_on,
         identity_dow, last_identity_on) in schedules {
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

        // P11 11.8: the weekly identity sweep, riding this same drawn slot --
        // one attempt, once a week, on `identity_dow`. It is not a run: it
        // writes no observations and stands in for nothing, so it happens
        // alongside the day's EOD/verify rather than instead of it. Isolated
        // like the gap backfill above: a failed sweep is reported and the day
        // proceeds.
        if weekly_slot_due(identity_dow, last_identity_on, today) {
            note += &run_identity_sweep(pool, cfg, sid, view_id, today).await;
        }

        // Amendment A1 stands: the run targets the previous trading day.
        // On the schedule's verify day, the same slot instead re-reads the
        // trailing five weekdays (kind backfill, trigger scheduled) so an
        // upstream restatement is actually seen. Budget-blocked verifies
        // degrade to the normal one-day run rather than blocking the day.
        let obs_date = previous_weekday(today);
        let want_verify = weekly_slot_due(verify_dow, last_verified_on, today);
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

/// One view's weekly identity sweep, reported as the note fragment the
/// schedule row carries (empty when there was nothing to say).
///
/// Never auto-confirms: anything above `BudgetLevel::Ok` skips the sweep and
/// says so, exactly as `run_gap_backfill` does. A sweep is a week's worth of
/// housekeeping -- next week is soon enough, and spending a user's confirmation
/// budget unattended is the one thing the scheduler must not do.
async fn run_identity_sweep(pool: &PgPool, cfg: &PipelineConfig, schedule_id: i64,
                            view_id: i64, today: NaiveDate) -> String
{
    async fn stamp(pool: &PgPool, schedule_id: i64, today: NaiveDate) {
        let _ = sqlx::query("UPDATE schedule SET last_identity_on = $2 WHERE id = $1")
            .bind(schedule_id).bind(today).execute(pool).await;
    }

    let batches = match crate::identity::plan_sweep(pool, view_id).await {
        Ok(b) => b,
        Err(e) => return format!("identity sweep failed: {e}; "),
    };
    if batches.is_empty() {
        // No class in this view opted in -- the default everywhere under
        // migration 0014. The week's slot is spent, quietly and for free.
        stamp(pool, schedule_id, today).await;
        return String::new();
    }
    let estimated = crate::identity::sweep_estimate(&batches);
    let today_total = match crate::budget::today_hits(pool).await {
        Ok(n) => n,
        Err(e) => return format!("identity sweep failed: {e}; "),
    };
    if crate::budget::check_level(estimated, today_total, cfg.soft_limit)
        != crate::budget::BudgetLevel::Ok
    {
        // Deliberately NOT stamped: the sweep did not happen, so tomorrow's
        // slot is not what should retry it -- next week's is, and leaving
        // `last_identity_on` alone is what makes that true.
        return format!("identity sweep skipped ({estimated} est. hits needs \
                        confirmation); ");
    }

    let fetcher = crate::master_fetch::BlpapiMasterFetcher { cfg, pool };
    match crate::identity::run_batches(pool, &fetcher, &batches, today).await {
        Ok(s) => {
            stamp(pool, schedule_id, today).await;
            format!("identity sweep: {} swept, {} triggered, {} anomalies; ",
                    s.swept, s.triggered, s.anomalies)
        }
        Err(e) => format!("identity sweep failed: {e}; "),
    }
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
    /// P11 11.5: `Some("2026-07")` on a **period-shaped** gap -- a periodic
    /// series whose period ended, produced no print, and is past grace. Its
    /// `start`/`end` are the period's real bounds, but the range is not a run
    /// of missing weekdays and must never be backfilled as one: the due-logic
    /// leg refetches it as a single ranged periodic request.
    ///
    /// `None` is the daily day-range gap, unchanged.
    pub period: Option<String>,
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
    // P11 11.5, THE fix: only **daily x history** non-text fields count toward
    // a day's coverage. A monthly NAV is absent from every weekday by design,
    // and a reference snapshot cannot be recovered for a past day at all; with
    // either counted, every date in a mixed view stayed permanently uncovered
    // and the scheduler re-bought the same window every single morning (the
    // P10 final review's "permanently-partial days" defect, which one non-daily
    // field turns from bounded into perpetual). The deliberate consequence: a
    // date is never uncovered because of a field backfill could not supply.
    for f in fields.iter().filter(|vf| crate::fetch::is_daily_history_parts(
        &vf.def.value_kind, &vf.def.fetch_via, &vf.effective_cadence)) {
        *expected.entry(f.def.asset_class_id).or_insert(0) += 1;
    }

    // The count below must be drawn from exactly the same population as
    // `expected`, or `have >= need` could be satisfied by a monthly print
    // standing in for a missing daily close.
    let rows: Vec<(i64, NaiveDate, i64)> = sqlx::query_as(
        "SELECT o.instrument_id, o.obs_date, count(DISTINCT o.field_id)::bigint
           FROM observation o
           JOIN view_instrument vi ON vi.instrument_id = o.instrument_id
                                  AND vi.view_id = $1
           JOIN view_field vf ON vf.view_id = vi.view_id AND vf.field_id = o.field_id
           JOIN field_def fd ON fd.id = o.field_id AND fd.value_kind <> 'text'
                            AND fd.fetch_via = 'history'
           JOIN asset_class ac ON ac.id = fd.asset_class_id
                              AND COALESCE(fd.cadence, ac.default_cadence) = 'daily'
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
                           start: s, end: e, period: None });
        }
    }

    // P11 11.5, the second detection arm: a periodic series' gap is
    // period-shaped. Grace is what makes it a gap rather than a fund simply
    // being a few days late -- the missing print is *fetched* the moment the
    // period ends (11.4), and only *reported* once it is anomalous.
    //
    // Note for callers: this arm is judged AS OF the `today` argument, which
    // for the daily arm is only a window edge. `run_gap_backfill` passes
    // `previous_weekday(today)` (its horizon against re-buying the day the EOD
    // run is about to fetch), so period grace is measured a day or two early
    // there -- harmless at monthly scale, and deliberately not special-cased,
    // but a trap for a future caller that hands this an arbitrary date.
    for miss in missing_periods(pool, view_id, today, PERIOD_LOOKBACK).await? {
        if !miss.overdue {
            continue;
        }
        out.push(Gap { instrument_id: miss.instrument_id, label: miss.label,
                       start: miss.start, end: miss.end, period: Some(miss.period) });
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

    /// The slot both weekly jobs ride. `identity_dow` ships NULL, so the
    /// off case is the one that must be unmistakable.
    #[test]
    fn a_weekly_slot_fires_on_its_weekday_and_only_once_that_day() {
        let thu = NaiveDate::from_ymd_opt(2026, 8, 20).unwrap(); // iso_dow 4
        assert!(!weekly_slot_due(None, None, thu), "NULL is off, however long ago");
        assert!(weekly_slot_due(Some(4), None, thu), "never run, and today is the day");
        assert!(!weekly_slot_due(Some(5), None, thu), "wrong weekday");
        assert!(!weekly_slot_due(Some(4), Some(thu), thu),
                "already done today -- a second heartbeat must not re-sweep");
        assert!(weekly_slot_due(Some(4), Some(thu - Duration::days(7)), thu),
                "last week's stamp does not cover this week");
        // A stamp in the future (clock moved back) is not "due", by the same
        // `< today` comparison verify has always used.
        assert!(!weekly_slot_due(Some(4), Some(thu + Duration::days(1)), thu));
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

    // ------------------------------------------------- P11 period arithmetic

    #[test]
    fn period_bounds_match_the_ends_the_budget_charges() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        // Monthly, including a leap February.
        assert_eq!(period_bounds(d(2026, 8, 12), "monthly"),
                   Some((d(2026, 8, 1), d(2026, 8, 31))));
        assert_eq!(period_bounds(d(2024, 2, 5), "monthly"),
                   Some((d(2024, 2, 1), d(2024, 2, 29))));
        // Quarterly: the calendar quarter containing the day.
        assert_eq!(period_bounds(d(2026, 8, 12), "quarterly"),
                   Some((d(2026, 7, 1), d(2026, 9, 30))));
        assert_eq!(period_bounds(d(2026, 12, 31), "quarterly"),
                   Some((d(2026, 10, 1), d(2026, 12, 31))));
        // Weekly ends on Friday, matching budget::periods_between's arm, and
        // a weekend day belongs to the week that has just closed.
        assert_eq!(period_bounds(d(2026, 8, 12), "weekly"),   // Wednesday
                   Some((d(2026, 8, 10), d(2026, 8, 14))));
        assert_eq!(period_bounds(d(2026, 8, 15), "weekly"),   // Saturday
                   Some((d(2026, 8, 10), d(2026, 8, 14))));
        // Structureless cadences have no periods at all.
        assert_eq!(period_bounds(d(2026, 8, 12), "daily"), None);
        assert_eq!(period_bounds(d(2026, 8, 12), "irregular"), None);
        // Exactly one period end lives inside each of these ranges, or the
        // due-logic and the budget would disagree about what a leg costs.
        for cadence in ["weekly", "monthly", "quarterly"] {
            let (s, e) = period_bounds(d(2026, 8, 12), cadence).unwrap();
            assert_eq!(crate::budget::periods_between(s, e, cadence), 1, "{cadence}");
        }
    }

    /// Probe F3: the print exists only AFTER the period ends. A period that
    /// ends today has not ended yet -- asking for it buys a row that does not
    /// exist, which is the one thing the cadence model must never do.
    #[test]
    fn completed_periods_exclude_the_period_still_running() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(completed_periods(d(2026, 8, 12), "monthly", 2),
                   vec![(d(2026, 7, 1), d(2026, 7, 31)),
                        (d(2026, 6, 1), d(2026, 6, 30))]);
        // The last day of August is still August.
        assert_eq!(completed_periods(d(2026, 8, 31), "monthly", 1),
                   vec![(d(2026, 7, 1), d(2026, 7, 31))]);
        // ... and the first of September is not.
        assert_eq!(completed_periods(d(2026, 9, 1), "monthly", 1),
                   vec![(d(2026, 8, 1), d(2026, 8, 31))]);
        // Year and quarter boundaries.
        assert_eq!(completed_periods(d(2026, 1, 15), "monthly", 1),
                   vec![(d(2025, 12, 1), d(2025, 12, 31))]);
        assert_eq!(completed_periods(d(2026, 8, 12), "quarterly", 2),
                   vec![(d(2026, 4, 1), d(2026, 6, 30)),
                        (d(2026, 1, 1), d(2026, 3, 31))]);
        // Saturday: the week that ended on Friday is complete.
        assert_eq!(completed_periods(d(2026, 8, 15), "weekly", 1),
                   vec![(d(2026, 8, 10), d(2026, 8, 14))]);
        assert!(completed_periods(d(2026, 8, 12), "daily", 2).is_empty());
    }

    #[test]
    fn period_labels_name_the_period_not_a_day_range() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        assert_eq!(period_label(d(2026, 7, 1), "monthly"), "2026-07");
        assert_eq!(period_label(d(2026, 7, 1), "quarterly"), "2026-Q3");
        assert_eq!(period_label(d(2026, 1, 1), "quarterly"), "2026-Q1");
        assert_eq!(period_label(d(2026, 8, 10), "weekly"), "2026-W33");
    }

    /// Grace is calendar days after the period ends, and it is INCLUSIVE of
    /// the boundary day: `today > end + grace`.
    #[test]
    fn grace_is_calendar_days_past_the_period_end() {
        let d = |y, m, day| NaiveDate::from_ymd_opt(y, m, day).unwrap();
        let july_end = d(2026, 7, 31);
        assert!(!period_overdue(july_end, 10, d(2026, 8, 5)));
        assert!(!period_overdue(july_end, 10, d(2026, 8, 10)), "the boundary day itself");
        assert!(period_overdue(july_end, 10, d(2026, 8, 11)));
        // Zero grace: late the day after the period closes.
        assert!(!period_overdue(july_end, 0, july_end));
        assert!(period_overdue(july_end, 0, d(2026, 8, 1)));
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
