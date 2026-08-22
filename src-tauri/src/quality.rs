//! P7: the quality gate. Structural validation (types, dates, security
//! errors) already lives in the sidecar and fetch::coerce; this module is
//! the missing judgment layer -- a value that arrived CLEANLY and still
//! looks wrong. Pure functions here; the DB runner lives below them.
//!
//! Every check is per-field opt-in (field_def.qc_*): whether a check makes
//! sense depends on what the field IS, and nothing here guesses from a
//! mnemonic. The one unconditional check is IEEE weirdness (NaN/inf), which
//! is wrong for every numeric field there is.

use chrono::NaiveDate;

#[derive(Debug, Clone, Copy, Default)]
pub struct QcConfig {
    pub nonpositive: bool,
    pub outlier_pct: Option<f64>,
    pub stale_days: Option<i32>,
}

impl QcConfig {
    pub fn enabled(&self) -> bool {
        self.nonpositive || self.outlier_pct.is_some() || self.stale_days.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesFinding {
    pub obs_date: NaiveDate,
    pub code: &'static str,
    pub detail: String,
}

/// Walk an ASCENDING current-raw series once; report only for dates inside
/// [from, to] -- the run being judged -- so history is context, not noise.
pub fn evaluate_series(
    cfg: &QcConfig,
    series: &[(NaiveDate, f64)],
    from: NaiveDate,
    to: NaiveDate,
) -> Vec<SeriesFinding> {
    let mut out = Vec::new();
    let in_range = |d: NaiveDate| d >= from && d <= to;
    let mut streak = 1usize;
    for (i, &(d, v)) in series.iter().enumerate() {
        let prev = (i > 0).then(|| series[i - 1]);
        if let Some((_, pv)) = prev {
            streak = if pv == v { streak + 1 } else { 1 };
        }
        if !in_range(d) {
            continue;
        }
        if !v.is_finite() {
            out.push(SeriesFinding {
                obs_date: d,
                code: "quality_not_finite",
                detail: format!("stored value {v} is not a finite number"),
            });
            continue; // the other checks are meaningless on NaN/inf
        }
        if cfg.nonpositive && v <= 0.0 {
            out.push(SeriesFinding {
                obs_date: d,
                code: "quality_nonpositive",
                detail: format!("value {v} is not positive"),
            });
        }
        if let (Some(pct), Some((pd, pv))) = (cfg.outlier_pct, prev) {
            if pv != 0.0 && pv.is_finite() {
                let mv = (v / pv - 1.0) * 100.0;
                if mv.abs() > pct {
                    out.push(SeriesFinding {
                        obs_date: d,
                        code: "quality_outlier",
                        detail: format!(
                            "moved {mv:.1}% vs {pd} ({pv} -> {v}), threshold {pct}%"
                        ),
                    });
                }
            }
        }
        if let Some(n) = cfg.stale_days {
            let n = n as usize;
            // Alert when the streak first reaches n, and keep alerting on the
            // newest point while it stays frozen (daily runs see the series
            // end); a backfill over the middle of a long streak stays quiet.
            if streak == n || (streak > n && i == series.len() - 1) {
                out.push(SeriesFinding {
                    obs_date: d,
                    code: "quality_stale",
                    detail: format!(
                        "unchanged for {streak} consecutive observations (threshold {n})"
                    ),
                });
            }
        }
    }
    out
}

/// P11 11.6: how many consecutive all-NIL weekdays stop looking like a
/// calendar and start looking like a hole in the licence.
///
/// Five, because no probed market closes for a whole trading week. A genuine
/// suspension does reach five and DESERVES the flag; a missing entitlement
/// always reaches it, and is otherwise perfectly silent -- probe F6 caught an
/// individual govvie returning NIL for 8/8 weekdays, every addressing form,
/// which today's rules would have filed as eight holidays and never mentioned
/// again.
pub const NIL_STREAK_WEEKDAYS: usize = 5;

/// How far back the streak walk reads evidence. A cap is needed because an
/// unentitled series never stops being NIL; it bounds the query and, with it,
/// the span the finding can report (the alarm fires at 5, so a truncated span
/// is a floor, never a miss).
const NIL_LOOKBACK_DAYS: i64 = 120;

/// The trailing run of consecutive WEEKDAYS ending at `anchor`, every one of
/// which is marked non-trading. Ascending; empty when `anchor` itself is not
/// marked.
///
/// Weekends are stepped over without breaking the run and without counting
/// toward it -- a market being shut on Saturday is not evidence of anything.
pub fn trailing_nil_weekdays(
    marked: &std::collections::HashSet<NaiveDate>,
    anchor: NaiveDate,
) -> Vec<NaiveDate> {
    let mut span = Vec::new();
    let mut day = anchor;
    while marked.contains(&day) {
        span.push(day);
        loop {
            day -= chrono::Duration::days(1);
            if !crate::scheduler::is_weekend(day) {
                break;
            }
        }
    }
    span.reverse();
    span
}

/// Instruments the run REQUESTED and Bloomberg answered with silence -- no
/// cell, no problem. A holiday is not silence (it arrives as no_data); this
/// is the partial-response case where a name simply vanished from the reply.
///
/// A request/session-level problem (`instrument_id: None`, e.g. a sidecar
/// failure) explains the silence for the WHOLE run, just not per name: if
/// any problem in the outcome is global, every instrument's silence is
/// already accounted for, so this returns empty rather than flagging each
/// requested instrument as individually unexplained.
pub fn unexplained_instruments(
    requested: &[i64],
    outcome: &crate::fetch::FetchOutcome,
) -> Vec<i64> {
    if outcome.problems.iter().any(|p| p.instrument_id.is_none()) {
        return Vec::new();
    }
    use std::collections::HashSet;
    let mut explained: HashSet<i64> = outcome.cells.iter().map(|c| c.instrument_id).collect();
    explained.extend(outcome.problems.iter().filter_map(|p| p.instrument_id));
    requested.iter().copied().filter(|id| !explained.contains(id)).collect()
}

// ---------------------------------------------------------------- DB runner

use crate::error::AppResult;
use crate::fetch::{FetchOutcome, FetchRequest};
use sqlx::PgPool;

/// Judge a run AFTER ingest committed, against what the database now holds:
/// the stored series is the single source the checks read, so a backfill and
/// an EOD run are judged identically. Findings are ingest_issue rows with
/// severity 'quality', attached to the run. Advisory by contract -- the
/// caller logs an error and keeps the run.
///
/// Findings are a per-run statement about the data as fetched by THAT run,
/// not a durable per-instrument fact deduped across runs. The Friday verify
/// run (see orchestrator::run_verify) re-fetches and re-judges its trailing
/// 5-weekday window, so any finding a daily run already reported inside that
/// window gets a second `ingest_issue` row attached to the verify run, and
/// the verify run lands 'partial' again for data a prior run already flagged.
/// This is DELIBERATE, not a bug to dedupe away: each run's findings answer
/// "what did this run see," and the verify run's whole point is to re-see.
pub async fn run_quality_gate(pool: &PgPool, run_id: i64, req: &FetchRequest,
                              outcome: &FetchOutcome) -> AppResult<u64> {
    let mut findings = 0u64;

    let requested: Vec<i64> = req.assets.iter().map(|a| a.instrument_id).collect();
    for iid in unexplained_instruments(&requested, outcome) {
        sqlx::query(
            "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
             VALUES ($1,$2,'quality','quality_no_response',
                     'requested in this run but Bloomberg returned neither data \
                      nor a problem for it')")
            .bind(run_id).bind(iid).execute(pool).await?;
        findings += 1;
    }

    // Independently fallible, and placed BEFORE the per-field QC block, so the
    // two cannot silence each other: the loudest alarm in the module is already
    // written when the QC queries run, and a failure of its own is logged and
    // stepped over instead of aborting the rest of the gate -- the same
    // log-and-continue the caller applies to the gate as a whole.
    match nil_streak_findings(pool, run_id, req).await {
        Ok(n) => findings += n,
        Err(e) => eprintln!("warning: nil_streak check failed for run {run_id}: {e}"),
    }

    // Which fields carry any check at all -- one query, not one per cell.
    let mut field_ids: Vec<i64> = outcome.cells.iter().map(|c| c.field_id).collect();
    field_ids.sort_unstable();
    field_ids.dedup();
    if field_ids.is_empty() {
        return Ok(findings);
    }
    let cfgs: Vec<(i64, bool, Option<f64>, Option<i32>)> = sqlx::query_as(
        "SELECT f.id, f.qc_nonpositive, f.qc_outlier_pct,
                COALESCE(f.qc_stale_days, ac.qc_stale_days_default) AS qc_stale_days
           FROM field_def f
           JOIN asset_class ac ON ac.id = f.asset_class_id
          WHERE f.id = ANY($1)")
        .bind(&field_ids).fetch_all(pool).await?;
    let cfg_of = |fid: i64| cfgs.iter()
        .find(|(id, ..)| *id == fid)
        .map(|&(_, n, o, s)| QcConfig { nonpositive: n, outlier_pct: o, stale_days: s })
        .unwrap_or_default();

    let mut pairs: Vec<(i64, i64)> = outcome.cells.iter()
        .map(|c| (c.instrument_id, c.field_id)).collect();
    pairs.sort_unstable();
    pairs.dedup();

    for (iid, fid) in pairs {
        let cfg = cfg_of(fid);
        if !cfg.enabled() {
            continue;
        }
        // Enough history for the stale streak plus the run's own range; the
        // series is judged ascending, so the DESC page is reversed.
        let span = (req.end - req.start).num_days().max(0) as i64;
        let window = (cfg.stale_days.unwrap_or(0) as i64 + span + 10).clamp(10, 200);
        let mut series: Vec<(chrono::NaiveDate, f64)> = sqlx::query_as(
            "SELECT obs_date, value_num FROM observation
              WHERE instrument_id = $1 AND field_id = $2
                AND layer = 'raw' AND granularity = 'eod'
                AND system_to = 'infinity' AND value_num IS NOT NULL
                AND obs_date <= $3
              ORDER BY obs_date DESC LIMIT $4")
            .bind(iid).bind(fid).bind(req.end).bind(window)
            .fetch_all(pool).await?;
        series.reverse();
        for f in evaluate_series(&cfg, &series, req.start, req.end) {
            sqlx::query(
                "INSERT INTO ingest_issue
                   (run_id, instrument_id, field_id, obs_date, severity, code, detail)
                 VALUES ($1,$2,$3,$4,'quality',$5,$6)")
                .bind(run_id).bind(iid).bind(fid).bind(f.obs_date)
                .bind(f.code).bind(&f.detail)
                .execute(pool).await?;
            findings += 1;
        }
    }
    Ok(findings)
}

/// P11 11.6, the NIL-streak alarm. Run as part of the quality gate, which the
/// orchestrator calls AFTER `ingest::record_non_trading_days` -- so "the
/// current fetch plus stored evidence" is simply the evidence table, with this
/// run's own marks already in it, and there is no second copy of Rules A/B
/// here to drift out of step with the first.
///
/// Scope, in the order the conditions have to be read:
/// * only a request carrying a daily x history field -- the only fetch whose
///   silence is evidence about trading sessions at all (11.6 gating);
/// * only instruments this run actually asked about (`req.assets`);
/// * only instruments with a mark inside the run's own window, which is what
///   makes this a statement about what THIS run saw rather than a nightly
///   re-reading of dormant history.
///
/// One finding per instrument per run: the evidence table is per instrument,
/// so the streak is too, and the detail names the span rather than the run
/// emitting one row per silent day.
///
/// The marks are NOT deleted or suppressed. That is the doctrine, not an
/// oversight: the evidence keeps the auto-backfill from re-buying an
/// unentitled series every night, and this finding is how a human hears about
/// it anyway.
async fn nil_streak_findings(pool: &PgPool, run_id: i64, req: &FetchRequest)
    -> AppResult<u64>
{
    use std::collections::{HashMap, HashSet};

    let daily_history = req.fields.iter().any(|f|
        crate::fetch::is_daily_history_parts(&f.value_kind, &f.fetch_via, &f.cadence));
    if !daily_history || req.assets.is_empty() {
        return Ok(0);
    }
    let ids: Vec<i64> = req.assets.iter().map(|a| a.instrument_id).collect();
    let from = req.end - chrono::Duration::days(NIL_LOOKBACK_DAYS);
    let rows: Vec<(i64, NaiveDate)> = sqlx::query_as(
        "SELECT instrument_id, obs_date FROM non_trading_day
          WHERE instrument_id = ANY($1) AND obs_date BETWEEN $2 AND $3")
        .bind(&ids).bind(from).bind(req.end).fetch_all(pool).await?;
    if rows.is_empty() {
        return Ok(0);
    }
    let mut marks: HashMap<i64, HashSet<NaiveDate>> = HashMap::new();
    for (iid, d) in rows {
        marks.entry(iid).or_default().insert(d);
    }

    let mut findings = 0u64;
    for a in &req.assets {
        let Some(seen) = marks.get(&a.instrument_id) else { continue };
        // The newest weekday this run covered that came back with no session.
        let Some(anchor) = seen.iter().copied()
            .filter(|d| *d >= req.start && !crate::scheduler::is_weekend(*d))
            .max() else { continue };
        let span = trailing_nil_weekdays(seen, anchor);
        if span.len() < NIL_STREAK_WEEKDAYS {
            continue;
        }
        let detail = format!(
            "{} consecutive weekdays with no session ({} .. {}), threshold {} -- \
             no market closes for a whole trading week, so this is a suspension or, \
             more likely, a missing historical entitlement for {} (probe F6). \
             The evidence rows stand; they are what stops the backfill re-buying it.",
            span.len(), span[0], anchor, NIL_STREAK_WEEKDAYS, a.bdp_security);
        sqlx::query(
            "INSERT INTO ingest_issue (run_id, instrument_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,'quality','nil_streak',$4)")
            .bind(run_id).bind(a.instrument_id).bind(anchor).bind(&detail)
            .execute(pool).await?;
        findings += 1;
    }
    Ok(findings)
}

/// P11 11.6: a periodic series whose period ended, passed its grace, and still
/// has no print -- "the June NAV never arrived", said once per period instead
/// of as a month of day-shaped gap noise.
///
/// Detection is NOT re-derived here: `scheduler::missing_periods` is the single
/// primitive behind the due-logic, the period-shaped gap and this finding
/// (controller ruling R2), and its `overdue` flag is the grace decision. This
/// only turns the overdue ones into P7 findings.
///
/// Called after ingest, so a period this very run just bought is not reported
/// late -- the print is already in `observation` when the misses are computed.
/// A finding is a per-run statement, like every other one in this module: the
/// July NAV stays overdue on every run until it arrives, and each of those runs
/// lands 'partial' by the standing `quality_findings > 0` rule. That is the
/// alert doing its job, and it is the ONLY status effect it has.
pub async fn record_publication_overdue(pool: &PgPool, run_id: i64, view_id: i64,
                                        today: NaiveDate) -> AppResult<u64> {
    let misses = crate::scheduler::missing_periods(
        pool, view_id, today, crate::scheduler::PERIOD_LOOKBACK).await?;
    let mut findings = 0u64;
    for m in misses.iter().filter(|m| m.overdue) {
        let detail = format!(
            "the {} period {} ({} .. {}) has no {} print for {} and is past its \
             publication grace -- the print is late, not the days inside it",
            m.cadence, m.period, m.start, m.end, m.mnemonic, m.label);
        sqlx::query(
            "INSERT INTO ingest_issue
               (run_id, instrument_id, field_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,$4,'quality','publication_overdue',$5)")
            .bind(run_id).bind(m.instrument_id).bind(m.field_id).bind(m.end)
            .bind(&detail)
            .execute(pool).await?;
        findings += 1;
    }
    Ok(findings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{CellProblem, CellValue, FetchOutcome, ObsCell};

    fn d(s: &str) -> NaiveDate {
        s.parse().unwrap()
    }
    fn cfg(nonpos: bool, outlier: Option<f64>, stale: Option<i32>) -> QcConfig {
        QcConfig { nonpositive: nonpos, outlier_pct: outlier, stale_days: stale }
    }

    #[test]
    fn nonpositive_and_not_finite_are_flagged_in_range_only() {
        let s = [
            (d("2026-08-10"), -1.0),
            (d("2026-08-11"), 0.0),
            (d("2026-08-12"), f64::NAN),
            (d("2026-08-13"), 10.0),
        ];
        let f = evaluate_series(&cfg(true, None, None), &s, d("2026-08-11"), d("2026-08-13"));
        // the 08-10 value is out of range; 08-11 nonpositive; 08-12 not finite
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].obs_date, f[0].code), (d("2026-08-11"), "quality_nonpositive"));
        assert_eq!((f[1].obs_date, f[1].code), (d("2026-08-12"), "quality_not_finite"));
    }

    #[test]
    fn outlier_compares_against_the_previous_observation() {
        let s = [
            (d("2026-08-10"), 100.0),
            (d("2026-08-11"), 100.5),
            (d("2026-08-12"), 145.0),
        ];
        let f = evaluate_series(&cfg(false, Some(30.0), None), &s, d("2026-08-12"), d("2026-08-12"));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].code, "quality_outlier");
        assert!(f[0].detail.contains("44.3%"), "detail: {}", f[0].detail);
        // a 30% threshold is not tripped by 0.5%
        let quiet = evaluate_series(&cfg(false, Some(30.0), None), &s, d("2026-08-11"), d("2026-08-11"));
        assert!(quiet.is_empty());
    }

    #[test]
    fn outlier_skips_a_zero_previous_value() {
        let s = [(d("2026-08-11"), 0.0), (d("2026-08-12"), 5.0)];
        assert!(evaluate_series(&cfg(false, Some(30.0), None), &s, d("2026-08-12"), d("2026-08-12")).is_empty());
    }

    #[test]
    fn stale_fires_when_the_streak_reaches_n_and_on_the_frozen_series_end() {
        let s = [
            (d("2026-08-10"), 7.0),
            (d("2026-08-11"), 7.0),
            (d("2026-08-12"), 7.0),
            (d("2026-08-13"), 7.0),
        ];
        // streak hits 3 on 08-12
        let at_n = evaluate_series(&cfg(false, None, Some(3)), &s, d("2026-08-12"), d("2026-08-12"));
        assert_eq!(at_n.len(), 1);
        assert_eq!(at_n[0].code, "quality_stale");
        // the next daily run (range = 08-13 only, streak 4 > n at series end)
        let next_day = evaluate_series(&cfg(false, None, Some(3)), &s, d("2026-08-13"), d("2026-08-13"));
        assert_eq!(next_day.len(), 1, "a still-frozen series keeps alarming");
        // a varied series never fires
        let varied = [(d("2026-08-10"), 7.0), (d("2026-08-11"), 7.1), (d("2026-08-12"), 7.0)];
        assert!(evaluate_series(&cfg(false, None, Some(2)), &varied, d("2026-08-10"), d("2026-08-12")).is_empty());
    }

    #[test]
    fn unexplained_silence_is_requested_minus_cells_minus_problems() {
        let out = FetchOutcome {
            cells: vec![ObsCell {
                instrument_id: 1,
                field_id: 9,
                obs_date: d("2026-08-12"),
                value: CellValue::Num(1.0),
            }],
            problems: vec![CellProblem {
                instrument_id: Some(2),
                field_id: None,
                obs_date: Some(d("2026-08-12")),
                code: "no_data".into(),
                detail: "holiday".into(),
            }],
        };
        assert_eq!(unexplained_instruments(&[1, 2, 3], &out), vec![3]);
        assert!(unexplained_instruments(&[1, 2], &out).is_empty());
    }

    /// 11.6: the walk counts WEEKDAYS. A Monday anchor reaches back through
    /// the weekend to the previous Friday without the weekend breaking the run
    /// or padding it -- Saturday is not evidence that a market is shut.
    #[test]
    fn the_nil_walk_steps_over_weekends_and_stops_at_the_first_traded_day() {
        let marked: std::collections::HashSet<NaiveDate> = [
            "2026-08-13", "2026-08-14", "2026-08-17", "2026-08-18",
        ].iter().map(|s| d(s)).collect();
        // Tue 08-18 back to Thu 08-13: four weekdays, the weekend stepped over.
        let span = trailing_nil_weekdays(&marked, d("2026-08-18"));
        assert_eq!(span, vec![d("2026-08-13"), d("2026-08-14"),
                              d("2026-08-17"), d("2026-08-18")]);
        assert!(span.len() < NIL_STREAK_WEEKDAYS, "four is under the alarm");

        // 08-12 traded, so it is the wall the walk stops at.
        assert!(!marked.contains(&d("2026-08-12")));
        // An unmarked anchor has no trailing run at all.
        assert!(trailing_nil_weekdays(&marked, d("2026-08-19")).is_empty());
    }

    /// A request/session-level problem (no instrument_id, e.g. a sidecar
    /// failure) explains the whole run's silence -- it must not leave every
    /// requested instrument looking individually unexplained.
    #[test]
    fn a_global_problem_explains_the_whole_runs_silence() {
        let out = FetchOutcome {
            cells: vec![],
            problems: vec![CellProblem {
                instrument_id: None,
                field_id: None,
                obs_date: None,
                code: "sidecar_failed".into(),
                detail: "the fetch sidecar exited before answering".into(),
            }],
        };
        assert!(unexplained_instruments(&[1, 2, 3], &out).is_empty());
    }
}
