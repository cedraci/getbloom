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

/// Instruments the run REQUESTED and Bloomberg answered with silence -- no
/// cell, no problem. A holiday is not silence (it arrives as no_data); this
/// is the partial-response case where a name simply vanished from the reply.
pub fn unexplained_instruments(
    requested: &[i64],
    outcome: &crate::fetch::FetchOutcome,
) -> Vec<i64> {
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

    // Which fields carry any check at all -- one query, not one per cell.
    let mut field_ids: Vec<i64> = outcome.cells.iter().map(|c| c.field_id).collect();
    field_ids.sort_unstable();
    field_ids.dedup();
    if field_ids.is_empty() {
        return Ok(findings);
    }
    let cfgs: Vec<(i64, bool, Option<f64>, Option<i32>)> = sqlx::query_as(
        "SELECT id, qc_nonpositive, qc_outlier_pct, qc_stale_days
           FROM field_def WHERE id = ANY($1)")
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
}
