use crate::error::AppResult;
use crate::fetch::{CellValue, FetchOutcome};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize)]
pub struct IngestSummary {
    pub inserted: u64,
    pub superseded: u64,
    pub unchanged: u64,
    pub issues: u64,
}

/// Write observations without ever destroying one.
///
/// The previous implementation ended in ON CONFLICT DO UPDATE, which silently
/// replaced yesterday's number with today's. That makes a corrected value
/// indistinguishable from an original one and makes point-in-time history
/// impossible. Here a changed value closes the old row's system_to and inserts
/// a new one beneath it; an unchanged value does nothing at all.
pub async fn ingest_outcome(pool: &PgPool, run_id: i64, outcome: &FetchOutcome)
    -> AppResult<IngestSummary>
{
    // The basis these values were actually fetched at: all four adjustment
    // flags false (see blp_fetch.build_request).
    let raw_basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis
          WHERE adj_normal = false AND adj_abnormal = false
            AND adj_split = false AND adj_follow_dpdf = false")
        .fetch_one(pool).await?;

    // The instrument's believed currency, per validity period, loaded once --
    // stamped on every numeric cell so the observation carries its unit.
    let ids: Vec<i64> = {
        let mut v: Vec<i64> = outcome.cells.iter().map(|c| c.instrument_id).collect();
        v.sort_unstable(); v.dedup(); v
    };
    let ccy_periods: Vec<(i64, String, chrono::NaiveDate, chrono::NaiveDate)> =
        if ids.is_empty() { Vec::new() } else {
            sqlx::query_as(
                "SELECT instrument_id, value, valid_from, valid_to
                   FROM instrument_attr
                  WHERE attr = 'currency' AND system_to = 'infinity'
                    AND instrument_id = ANY($1)")
                .bind(&ids).fetch_all(pool).await?
        };
    let currency_at = |iid: i64, d: chrono::NaiveDate| -> Option<&str> {
        ccy_periods.iter()
            .find(|(i, _, from, to)| *i == iid && *from <= d && *to > d)
            // An instrument resolved without a listing date starts its belief
            // at the add date; the first EOD run observes YESTERDAY. The
            // earliest belief extends backward -- but never forward past a
            // death-capped valid_to.
            .or_else(|| ccy_periods.iter()
                .filter(|(i, _, from, _)| *i == iid && d < *from)
                .min_by_key(|(_, _, from, _)| *from))
            .map(|(_, v, _, _)| v.as_str())
    };

    // A restatement is legitimate -- and invisible unless said. The run stays
    // ok/partial on its own merits; this row is the audit trail's headline,
    // not a failure. Shared by the value_superseded and currency_changed arms
    // below.
    let describe = |n: &Option<f64>, t: &Option<String>| match (n, t) {
        (Some(v), _) => v.to_string(),
        (_, Some(s)) => format!("{s:?}"),
        _ => "NULL".into(),
    };

    let mut tx = pool.begin().await?;
    let (mut inserted, mut superseded, mut unchanged) = (0u64, 0u64, 0u64);

    for c in &outcome.cells {
        // Only a numeric price has an adjustment basis (schema
        // observation_numeric_needs_basis / the migration's "text-valued
        // fields ... legitimately have none"). Asserting RAW for a text cell
        // would be a false claim, and would also let a text row and a future
        // NULL-basis writer both claim "current" for the same logical series
        // without colliding on observation_current.
        let (num, text, basis_id) = match &c.value {
            CellValue::Num(n) => (Some(*n), None, Some(raw_basis)),
            CellValue::Text(t) => (None, Some(t.clone()), None),
        };
        let ccy = num.is_some().then(|| currency_at(c.instrument_id, c.obs_date)).flatten();

        // FOR UPDATE: two concurrent runs racing the same
        // (instrument, field, date, ..., basis) key must serialize here,
        // not both decide "no current row" and collide on
        // observation_current.
        let current: Option<(i64, Option<f64>, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT id, value_num, value_text, currency FROM observation
              WHERE instrument_id = $1 AND field_id = $2 AND obs_date = $3
                AND granularity = 'eod' AND layer = 'raw'
                AND basis_id IS NOT DISTINCT FROM $4
                AND system_to = 'infinity'
              FOR UPDATE")
            .bind(c.instrument_id).bind(c.field_id).bind(c.obs_date).bind(basis_id)
            .fetch_optional(&mut *tx).await?;

        if let Some((id, old_num, old_text, old_ccy)) = current {
            let same_value = old_num == num && old_text == text;
            if same_value && old_ccy.as_deref() == ccy {
                unchanged += 1;
                continue;
            }
            sqlx::query("UPDATE observation SET system_to = now() WHERE id = $1")
                .bind(id).execute(&mut *tx).await?;
            let (code, detail) = if same_value {
                ("currency_changed", format!(
                    "currency changed {} -> {} with the value unchanged -- \
                     redenomination or master-data correction",
                    old_ccy.as_deref().unwrap_or("(none)"),
                    ccy.unwrap_or("(none)")))
            } else {
                ("value_superseded", format!("stored value {} superseded by {}",
                    describe(&old_num, &old_text), describe(&num, &text)))
            };
            sqlx::query(
                "INSERT INTO ingest_issue
                   (run_id, instrument_id, field_id, obs_date, severity, code, detail)
                 VALUES ($1,$2,$3,$4,'warn',$5,$6)")
                .bind(run_id).bind(c.instrument_id).bind(c.field_id).bind(c.obs_date)
                .bind(code).bind(&detail)
                .execute(&mut *tx).await?;
            superseded += 1;
        }

        sqlx::query(
            "INSERT INTO observation
               (instrument_id, field_id, obs_date, granularity, layer, basis_id,
                value_num, value_text, run_id, currency)
             VALUES ($1,$2,$3,'eod','raw',$4,$5,$6,$7,$8)")
            .bind(c.instrument_id).bind(c.field_id).bind(c.obs_date)
            .bind(basis_id).bind(num).bind(text).bind(run_id).bind(ccy)
            .execute(&mut *tx).await?;
        inserted += 1;
    }

    for p in &outcome.problems {
        sqlx::query(
            "INSERT INTO ingest_issue
               (run_id, instrument_id, field_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,$4,'warn',$5,$6)")
            .bind(run_id).bind(p.instrument_id).bind(p.field_id).bind(p.obs_date)
            .bind(&p.code).bind(&p.detail)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(IngestSummary { inserted, superseded, unchanged,
                       issues: outcome.problems.len() as u64 })
}

/// Evidence-based non-trading days -- no external holiday calendar exists in
/// this system, so a day is recorded only when Bloomberg itself said there
/// was no session:
/// - rule A: a (instrument, day) with zero cells, >=1 dated `no_data`, and
///   no other-coded dated problem;
/// - rule B (multi-day ranges): ACTIVE_DAYS_ONLY omits non-trading days
///   silently, so a weekday with zero cells and zero dated problems, for an
///   instrument that returned cells elsewhere in the range, is non-trading
///   by inference (this also covers per-security suspensions, which equally
///   have no price to backfill).
///
/// P11 11.6, gating: both rules are about a DAILY history fetch, and only
/// about one. A monthly NAV prints once and is silent on the other twenty
/// weekdays BY DESIGN; reading that silence as evidence would fabricate ~240
/// fake holidays a year per fund AND permanently suppress the period's real
/// gap -- the table would stop meaning "Bloomberg said this instrument had no
/// session that day". Two gates, coarse and fine:
///
/// * the orchestrator does not call this at all when the run planned no daily
///   history spec (R3: a history spec with no `periodicity`);
/// * here, per (instrument, date): anything a periodic leg of THIS request
///   covers is that leg's business, not the daily leg's. The exclusion is
///   derived from `req.periodic` -- the legs already carry their own
///   securities and period range -- so no second flag exists to drift out of
///   step with what was actually asked (controller ruling R3).
pub async fn record_non_trading_days(pool: &PgPool, req: &crate::fetch::FetchRequest,
                                     outcome: &FetchOutcome) -> AppResult<u64> {
    use chrono::NaiveDate;
    use std::collections::{HashMap, HashSet};

    // Is this (instrument, date) inside a periodic leg's range for that very
    // instrument? A neighbour's period says nothing about a name the leg does
    // not carry, so the instrument list is part of the test, not just the
    // dates.
    let shadowed = |iid: i64, d: NaiveDate| -> bool {
        req.periodic.iter().any(|l|
            d >= l.start && d <= l.end && l.instrument_ids.contains(&iid))
    };

    // Periodic cells are excluded here too, and for the same reason they are
    // excluded from the marks: a monthly print is not proof that the
    // instrument "answered" for the daily range, which is exactly what rule B
    // would otherwise take it for.
    let mut cells: HashMap<i64, HashSet<NaiveDate>> = HashMap::new();
    for c in &outcome.cells {
        if shadowed(c.instrument_id, c.obs_date) {
            continue;
        }
        cells.entry(c.instrument_id).or_default().insert(c.obs_date);
    }
    let mut no_data: HashSet<(i64, NaiveDate)> = HashSet::new();
    let mut other: HashSet<(i64, NaiveDate)> = HashSet::new();
    for p in &outcome.problems {
        if let (Some(iid), Some(d)) = (p.instrument_id, p.obs_date) {
            if p.code == "no_data" { no_data.insert((iid, d)); }
            else { other.insert((iid, d)); }
        }
    }

    let mut marks: Vec<(i64, NaiveDate, &'static str)> = Vec::new();
    for &(iid, d) in &no_data {
        let has_cell = cells.get(&iid).is_some_and(|s| s.contains(&d));
        if !has_cell && !other.contains(&(iid, d)) && !shadowed(iid, d) {
            marks.push((iid, d, "no_data"));
        }
    }
    if req.start < req.end {
        for (&iid, have) in &cells {
            let mut day = req.start;
            while day <= req.end {
                if !crate::scheduler::is_weekend(day)
                    && !have.contains(&day)
                    && !no_data.contains(&(iid, day))
                    && !other.contains(&(iid, day))
                    && !shadowed(iid, day) {
                    marks.push((iid, day, "range_inference"));
                }
                day += chrono::Duration::days(1);
            }
        }
    }

    let mut inserted = 0u64;
    for (iid, d, src) in marks {
        let r = sqlx::query(
            "INSERT INTO non_trading_day (instrument_id, obs_date, source)
             VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(iid).bind(d).bind(src).execute(pool).await?;
        inserted += r.rows_affected();
    }
    Ok(inserted)
}
