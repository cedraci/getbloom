//! P11 11.8: the weekly identity sweep -- the P9 rider, designed at last
//! (docs/superpowers/specs/2026-08-22-p11-cadence-and-fetch-capability-design.md).
//!
//! P6/P9 built everything that HAPPENS to a dead instrument: the retirement
//! path, the M&A investigation, the routing between them. What it never had
//! was a way to find out. Its one trigger is "stopped producing observations
//! for a week", which a matured bond or a delisted share class can dodge
//! indefinitely -- the security keeps answering, the book keeps paying for it.
//!
//! This module is the missing trigger and nothing else: one batched
//! ReferenceDataRequest per swept asset class, a verdict per security, and a
//! hand-off to `lifecycle::investigate`. It builds no lifecycle of its own.
//!
//! Three probe findings shape it and are worth stating where the code is:
//!
//! * **F5** -- spot FX/metals report MATURITY as the rolling T+2 SETTLEMENT
//!   date. Nothing here inspects a date to decide whether it looks like a
//!   settlement: the guard is that `identity_sweep` defaults to `'none'` and
//!   `'maturity'` is never configured on a spot class, so that date is never
//!   fetched. A heuristic would have to guess, and a wrong guess retires a
//!   live series.
//! * **F9** -- `field_not_applicable` on a single sweep field is NORMAL (an
//!   open-end fund has no INACTIVE_DATE). Verdicts are taken on whichever
//!   fields answered; only a security where every field failed is an anomaly,
//!   and an anomaly is advisory, never a retirement.
//! * **The budget** -- the sweep charges `purpose = 'identity'` at the wire
//!   seam (`BlpapiMasterFetcher::identity_sweep`), the corp-actions precedent:
//!   one charge, where the request is, with no estimate leg to double-count.

use crate::error::AppResult;
use crate::lifecycle::LifecycleSummary;
use crate::master_fetch::{self, MasterFetcher, MARKET_STATUS_ACTIVE};
use chrono::NaiveDate;
use sqlx::PgPool;
use std::collections::BTreeMap;

/// The class setting that opts out -- and the default every existing class
/// carries out of migration 0014.
pub const SWEEP_NONE: &str = "none";

/// Every sweep field for one security failed. Durable and advisory: silence
/// from Bloomberg is not evidence of death, but a security that answers
/// nothing week after week is worth a human's attention.
pub const NO_ANSWER_CODE: &str = "identity_sweep_no_answer";

/// One asset class's worth of sweep: ONE request, because the field set is
/// per class (F5) and batching across classes would have to ask every field
/// of every security.
#[derive(Debug, Clone)]
pub struct SweepBatch {
    pub asset_class_id: i64,
    pub class_name: String,
    /// `asset_class.identity_sweep`, never `'none'`.
    pub sweep: String,
    /// `(instrument_id, bdp_security)` for the view's active members of this
    /// class that hold a security string valid today.
    pub members: Vec<(i64, String)>,
}

#[derive(Debug, Default, serde::Serialize)]
pub struct SweepSummary {
    /// Requests that reached the wire: one per swept class, split again only
    /// if a class holds more than the transport's 100-security ceiling.
    pub batches: usize,
    /// Securities asked about.
    pub swept: usize,
    /// Securities whose trigger fired.
    pub triggered: usize,
    /// Of those, the ones a P6 cooldown had already investigated recently.
    pub cooldown_skipped: usize,
    /// Securities where EVERY sweep field failed (F9).
    pub anomalies: usize,
    /// What the P9 lifecycle did with the triggered names.
    pub lifecycle: LifecycleSummary,
}

/// What the returned fields say about one security.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Nothing in what came back says this instrument has ended.
    Alive,
    /// Dead, with the reason in the caller's words -- it reaches the user in
    /// the lifecycle issue and nothing branches on it.
    Dead(String),
    /// F9: not one sweep field answered. Advisory, never a retirement.
    NoAnswer,
}

/// The trigger rules, spec 11.8's table in code:
///
/// * `'market_status'`: `MARKET_STATUS <> 'ACTV'`, or INACTIVE_DATE set.
/// * `'maturity'`: any returned date on or before today.
///
/// A field that did not answer is silent, not false -- F9. That is why the
/// maturity arm reads "any date that came back", not "all three".
pub fn evaluate(sweep: &str, fields: &BTreeMap<String, String>, today: NaiveDate)
    -> Verdict
{
    if fields.is_empty() {
        return Verdict::NoAnswer;
    }
    match sweep {
        "market_status" => {
            if let Some(status) = fields.get(master_fetch::MARKET_STATUS_FIELD) {
                if status != MARKET_STATUS_ACTIVE {
                    return Verdict::Dead(format!("MARKET_STATUS {status}"));
                }
            }
            // "Set", not "past": Bloomberg publishes INACTIVE_DATE when it
            // knows the security stops, which can be a few days out. A
            // scheduled delisting is still a delisting.
            match fields.get("INACTIVE_DATE").and_then(|v| parse_date(v)) {
                Some(d) => Verdict::Dead(format!("INACTIVE_DATE {d}")),
                None => Verdict::Alive,
            }
        }
        "maturity" => {
            for field in master_fetch::MATURITY_SWEEP_FIELDS {
                let Some(d) = fields.get(field).and_then(|v| parse_date(v)) else {
                    continue;
                };
                if d <= today {
                    return Verdict::Dead(format!("{field} {d}"));
                }
            }
            Verdict::Alive
        }
        // Unreachable through `run_sweep`, which never plans a 'none' class.
        // Refusing rather than defaulting keeps a future third mode from
        // silently inheriting the equity rules.
        _ => Verdict::Alive,
    }
}

/// Bloomberg dates arrive ISO from the sidecar. An unparsable value is
/// treated as silence: it is not evidence of anything, and reading it as a
/// trigger would retire a series on a formatting change.
fn parse_date(v: &str) -> Option<NaiveDate> {
    v.trim().parse().ok()
}

/// What this view would ask, one batch per swept class. Empty is the normal
/// answer under migration 0014's defaults -- which is what makes the sweep
/// ship off.
///
/// Members come from `views::view_instruments`, so a retired book entry and an
/// instrument still under resolution review are excluded for the same reasons
/// the gap detector excludes them: neither is supposed to be collecting, so
/// neither is worth a hit.
pub async fn plan_sweep(pool: &PgPool, view_id: i64) -> AppResult<Vec<SweepBatch>> {
    let members = crate::views::view_instruments(pool, view_id).await?;
    if members.is_empty() {
        return Ok(Vec::new());
    }
    let classes: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT id, name, identity_sweep FROM asset_class WHERE identity_sweep <> $1
          ORDER BY id")
        .bind(SWEEP_NONE).fetch_all(pool).await?;

    Ok(classes.into_iter().filter_map(|(asset_class_id, class_name, sweep)| {
        let members: Vec<(i64, String)> = members.iter()
            .filter(|m| m.asset_class_id == asset_class_id)
            // No security string valid today = nothing to ask Bloomberg about.
            .filter_map(|m| m.security.clone().map(|s| (m.instrument_id, s)))
            .collect();
        (!members.is_empty())
            .then_some(SweepBatch { asset_class_id, class_name, sweep, members })
    }).collect())
}

/// What the planned batches would cost, for the scheduler's budget gate.
/// Priced the same way the seam charges: securities x that sweep's fields.
pub fn sweep_estimate(batches: &[SweepBatch]) -> i64 {
    batches.iter()
        .map(|b| master_fetch::identity_sweep_hit_cost(b.members.len(), &b.sweep))
        .sum()
}

/// The sweep: plan, ask, judge, hand off.
///
/// Per-security failures are isolated exactly as `lifecycle::run_check`
/// isolates them -- one name's bad day must not hide another's maturity --
/// and a whole class's request failing leaves the other classes to run.
pub async fn run_sweep<F: MasterFetcher>(pool: &PgPool, fetcher: &F, view_id: i64,
                                         today: NaiveDate)
    -> AppResult<SweepSummary>
{
    let batches = plan_sweep(pool, view_id).await?;
    run_batches(pool, fetcher, &batches, today).await
}

/// `run_sweep` on batches already planned -- the scheduler's entry point,
/// which has to price the plan through the budget gate before spending it and
/// must not re-plan (and risk pricing one plan while running another).
pub async fn run_batches<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                           batches: &[SweepBatch], today: NaiveDate)
    -> AppResult<SweepSummary>
{
    let mut summary = SweepSummary::default();
    for batch in batches {
        if let Err(e) = sweep_class(pool, fetcher, batch, today, &mut summary).await {
            eprintln!("identity sweep: class {} failed: {e}", batch.class_name);
        }
    }
    Ok(summary)
}

async fn sweep_class<F: MasterFetcher>(pool: &PgPool, fetcher: &F, batch: &SweepBatch,
                                       today: NaiveDate, summary: &mut SweepSummary)
    -> AppResult<()>
{
    // One request per class is the design; the chunking below is the
    // transport's 100-security ceiling, the same one `lifecycle::run_check`
    // and the EOD planner respect.
    for chunk in batch.members.chunks(crate::fetch::MAX_SECURITIES_PER_REQUEST) {
        let securities: Vec<String> = chunk.iter().map(|(_, s)| s.clone()).collect();
        let answered = fetcher.identity_sweep(&securities, &batch.sweep).await?;
        summary.batches += 1;
        let by_security: std::collections::HashMap<&str, &BTreeMap<String, String>> =
            answered.parsed.iter()
                .map(|a| (a.security.as_str(), &a.fields)).collect();

        for (instrument_id, security) in chunk {
            summary.swept += 1;
            // A security absent from the reply answered nothing at all, which
            // is the same anomaly as one whose every field failed.
            let empty = BTreeMap::new();
            let fields = by_security.get(security.as_str()).copied().unwrap_or(&empty);
            match evaluate(&batch.sweep, fields, today) {
                Verdict::Alive => {}
                Verdict::NoAnswer => {
                    summary.anomalies += 1;
                    let detail = format!(
                        "{security}: the identity sweep asked {} and not one field \
                         answered. Per-field 'not applicable' is normal (an open-end \
                         fund has no INACTIVE_DATE) -- a security answering NOTHING is \
                         not, and this one is neither confirmed alive nor retired. \
                         Check the security string and the class's identity_sweep \
                         setting.",
                        master_fetch::sweep_fields(&batch.sweep).join(", "));
                    if let Err(e) = crate::lifecycle::record_issue(
                        pool, *instrument_id, NO_ANSWER_CODE, &detail,
                        &mut summary.lifecycle).await {
                        eprintln!("identity sweep: recording {security}'s anomaly \
                                   failed: {e}");
                    }
                }
                Verdict::Dead(reason) => {
                    summary.triggered += 1;
                    if let Err(e) = dispatch(pool, fetcher, *instrument_id, security,
                                             &reason, fields, today, summary).await {
                        eprintln!("identity sweep: retiring {security} failed: {e}");
                    }
                }
            }
        }
    }
    Ok(())
}

/// Hand one dead instrument to the P9 lifecycle, honouring P6's cooldown.
///
/// The cooldown matters because the two finders overlap: P6 asks MARKET_STATUS
/// of a name that stopped printing, this asks every swept name every week, and
/// an equity investigation costs an `ma_deals` list plus up to three
/// `action_terms` reads. Without the shared cooldown a dead equity that the
/// user has not yet retired would re-buy that investigation every single week
/// -- the exact standing cost this whole feature exists to end.
///
/// The verbatim MARKET_STATUS is recorded first, when the sweep learned one,
/// because that attr IS the cooldown and it is the same fact from the same
/// field P6 records. The maturity arm never sees MARKET_STATUS and so writes
/// nothing: its dead names cost one cheap `retire_path` per week until the
/// user retires the book entry, which is what the issue tells them to do.
#[allow(clippy::too_many_arguments)]
async fn dispatch<F: MasterFetcher>(pool: &PgPool, fetcher: &F, instrument_id: i64,
                                    security: &str, reason: &str,
                                    fields: &BTreeMap<String, String>,
                                    today: NaiveDate, summary: &mut SweepSummary)
    -> AppResult<()>
{
    if crate::lifecycle::recently_checked(pool, instrument_id, today).await? {
        summary.cooldown_skipped += 1;
        return Ok(());
    }
    if let Some(status) = fields.get(master_fetch::MARKET_STATUS_FIELD) {
        crate::lifecycle::record_status(pool, instrument_id, status, today).await?;
    }
    crate::lifecycle::investigate(pool, fetcher, instrument_id, security, reason,
                                  today, &mut summary.lifecycle).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }
    fn today() -> NaiveDate { "2026-08-20".parse().unwrap() }

    #[test]
    fn market_status_triggers_on_a_non_active_status_or_a_set_inactive_date() {
        assert_eq!(evaluate("market_status", &f(&[("MARKET_STATUS", "ACTV")]), today()),
                   Verdict::Alive);
        assert!(matches!(
            evaluate("market_status", &f(&[("MARKET_STATUS", "ACQU")]), today()),
            Verdict::Dead(_)), "the live-measured dead-fund answer");
        // INACTIVE_DATE alone is enough, even while the status still says ACTV
        // -- and even dated in the future: a scheduled delisting is a delisting.
        assert!(matches!(
            evaluate("market_status",
                     &f(&[("MARKET_STATUS", "ACTV"), ("INACTIVE_DATE", "2026-12-31")]),
                     today()),
            Verdict::Dead(_)));
    }

    /// F9, the fund case: INACTIVE_DATE is `field_not_applicable` on an
    /// open-end fund, so the verdict rests on MARKET_STATUS alone.
    #[test]
    fn a_verdict_is_taken_on_whichever_fields_answered() {
        assert_eq!(evaluate("market_status", &f(&[("MARKET_STATUS", "ACTV")]), today()),
                   Verdict::Alive, "a missing INACTIVE_DATE is silence, not death");
        assert_eq!(evaluate("maturity", &f(&[("MATURITY", "2036-08-15")]), today()),
                   Verdict::Alive, "a missing CALLED_DT is silence, not a call");
        assert_eq!(evaluate("market_status", &f(&[]), today()), Verdict::NoAnswer);
        assert_eq!(evaluate("maturity", &f(&[]), today()), Verdict::NoAnswer);
    }

    #[test]
    fn maturity_triggers_on_any_returned_date_at_or_before_today() {
        assert_eq!(evaluate("maturity", &f(&[("MATURITY", "2036-08-15")]), today()),
                   Verdict::Alive);
        assert!(matches!(evaluate("maturity", &f(&[("MATURITY", "2026-08-19")]), today()),
                         Verdict::Dead(_)));
        assert!(matches!(evaluate("maturity", &f(&[("MATURITY", "2026-08-20")]), today()),
                         Verdict::Dead(_)), "matured today is matured");
        // A long-dated bond called early: CALLED_DT is the trigger, not MATURITY.
        assert!(matches!(
            evaluate("maturity",
                     &f(&[("MATURITY", "2036-08-15"), ("CALLED_DT", "2026-06-01")]),
                     today()),
            Verdict::Dead(_)));
    }

    /// Probe F5, stated as a test even though the real guard is upstream: the
    /// maturity rules WOULD retire a spot pair whose MATURITY is the T+2
    /// settlement date if it ever matured -- which is precisely why 'maturity'
    /// is never configured on a spot class and 'none' asks nothing.
    #[test]
    fn a_settlement_date_two_days_out_is_not_a_trigger_and_none_never_judges() {
        assert_eq!(evaluate("maturity", &f(&[("MATURITY", "2026-08-24")]), today()),
                   Verdict::Alive, "T+2 is in the future; the sweep is not what saves us");
        assert_eq!(evaluate("none", &f(&[("MATURITY", "2026-08-19")]), today()),
                   Verdict::Alive,
                   "an opted-out class has no rules, so it can reach no verdict");
        assert!(master_fetch::sweep_fields("none").is_empty(),
                "and it asks nothing, which is the guard that actually holds");
    }

    #[test]
    fn an_unparsable_date_is_silence_rather_than_a_retirement() {
        assert_eq!(evaluate("maturity", &f(&[("MATURITY", "N.A.")]), today()),
                   Verdict::Alive);
        assert_eq!(evaluate("market_status",
                            &f(&[("MARKET_STATUS", "ACTV"), ("INACTIVE_DATE", "")]),
                            today()),
                   Verdict::Alive);
    }

    #[test]
    fn the_estimate_prices_each_class_by_its_own_field_set() {
        let batch = |sweep: &str, n: usize| SweepBatch {
            asset_class_id: 1, class_name: "C".into(), sweep: sweep.into(),
            members: (0..n as i64).map(|i| (i, format!("S{i}"))).collect(),
        };
        assert_eq!(sweep_estimate(&[batch("market_status", 10)]), 20);
        assert_eq!(sweep_estimate(&[batch("maturity", 10)]), 30);
        assert_eq!(sweep_estimate(&[batch("market_status", 10), batch("maturity", 10)]),
                   50);
        assert_eq!(sweep_estimate(&[]), 0);
    }
}
