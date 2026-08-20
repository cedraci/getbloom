//! P4: the adjustment engine (design:
//! docs/superpowers/specs/2026-08-21-p4-adjustment-engine-design.md).
//!
//! Derives split-adjusted and net (split + dividend) series ON READ from the
//! stored RAW observations and the corp_action factor chain. Nothing here
//! writes anything: an amended factor changes every future read, and the
//! stored history stays pure fact.

use chrono::NaiveDate;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum AdjustMode {
    /// The stored values, untouched.
    Raw,
    /// Only flag-3 factors (capital changes: prices AND volumes).
    Splits,
    /// Every factor -- the "net price" series (DPDF with dividends).
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesKind { Price, Volume }

/// A volume series takes the OPPOSITE operator (P0 10.1) and only ever
/// receives flag-3 events. Everything else numeric is treated as a price.
pub fn series_kind(mnemonic: &str) -> SeriesKind {
    if mnemonic.to_ascii_uppercase().contains("VOLUME") {
        SeriesKind::Volume
    } else {
        SeriesKind::Price
    }
}

#[derive(Debug, Clone, Copy)]
pub struct FactorEvent {
    pub event_date: NaiveDate,
    pub amount: f64,
    pub operator: i16,
    pub flag: i16,
}

/// Apply every admitted event with `event_date > obs_date`, in chronological
/// order (`events` must arrive sorted ascending). Order only matters when an
/// additive operator (3) mixes with multiplicative ones -- rare, but pinned
/// by test rather than assumed away.
pub fn apply_chain(kind: SeriesKind, mode: AdjustMode,
                   obs_date: NaiveDate, raw: f64, events: &[FactorEvent]) -> f64 {
    if mode == AdjustMode::Raw {
        return raw;
    }
    let mut v = raw;
    for e in events {
        if e.event_date <= obs_date {
            continue; // the event itself and everything after it stay as-is
        }
        let admitted = match (mode, kind) {
            // Flag 1 factors adjust prices only, whatever the mode.
            (_, SeriesKind::Volume) => e.flag == 3,
            (AdjustMode::Splits, SeriesKind::Price) => e.flag == 3,
            (AdjustMode::All, SeriesKind::Price) => true,
            (AdjustMode::Raw, _) => unreachable!(),
        };
        if !admitted {
            continue;
        }
        v = match (e.operator, kind) {
            // P0 10.1: div/mult/add for prices, the OPPOSITE for volumes.
            (1, SeriesKind::Price) => v / e.amount,
            (2, SeriesKind::Price) => v * e.amount,
            (3, SeriesKind::Price) => v + e.amount,
            (1, SeriesKind::Volume) => v * e.amount,
            (2, SeriesKind::Volume) => v / e.amount,
            (3, SeriesKind::Volume) => v - e.amount,
            _ => v, // unknown operator: leave untouched (counted upstream)
        };
    }
    v
}

// ---------------------------------------------------------------- DB loader

#[derive(Debug, serde::Serialize)]
pub struct AdjRow {
    pub obs_date: NaiveDate,
    pub raw: f64,
    pub adjusted: f64,
}

#[derive(Debug, serde::Serialize)]
pub struct AdjSeries {
    /// obs_date DESC, current rows only -- a derived series has no
    /// supersession history of its own.
    pub rows: Vec<AdjRow>,
    /// Events admitted by mode and kind (i.e. applied to at least the
    /// oldest observation).
    pub factors_used: usize,
    /// Stored factor rows whose typed columns are incomplete (unparsed
    /// payload): excluded, and the reader deserves to know.
    pub unusable_factors: usize,
}

/// Load the current RAW observations and the current factor chain, and
/// derive. Read-only; never calls Bloomberg.
pub async fn adjusted_series(pool: &sqlx::PgPool, instrument_id: i64,
                             field_id: i64, mode: AdjustMode, limit: i64)
    -> crate::error::AppResult<AdjSeries>
{
    let mnemonic: String = sqlx::query_scalar(
        "SELECT mnemonic FROM field_def WHERE id = $1")
        .bind(field_id).fetch_one(pool).await?;
    let kind = series_kind(&mnemonic);

    let obs: Vec<(NaiveDate, f64)> = sqlx::query_as(
        "SELECT obs_date, value_num FROM observation
          WHERE instrument_id = $1 AND field_id = $2
            AND layer = 'raw' AND system_to = 'infinity'
            AND value_num IS NOT NULL
          ORDER BY obs_date DESC
          LIMIT $3")
        .bind(instrument_id).bind(field_id).bind(limit.clamp(1, 5000))
        .fetch_all(pool).await?;

    let factor_rows: Vec<(Option<NaiveDate>, Option<f64>, Option<i16>, Option<i16>)> =
        sqlx::query_as(
            "SELECT event_date, amount, operator, flag FROM corp_action
              WHERE instrument_id = $1 AND source_field = $2
                AND system_to = 'infinity'
              ORDER BY event_date ASC NULLS LAST")
            .bind(instrument_id).bind(crate::corp_actions::FACTOR_FIELD)
            .fetch_all(pool).await?;
    let mut events = Vec::with_capacity(factor_rows.len());
    let mut unusable = 0usize;
    for row in factor_rows {
        match row {
            (Some(event_date), Some(amount), Some(operator), Some(flag)) =>
                events.push(FactorEvent { event_date, amount, operator, flag }),
            _ => unusable += 1,
        }
    }

    let factors_used = if mode == AdjustMode::Raw { 0 } else {
        events.iter().filter(|e| match kind {
            SeriesKind::Volume => e.flag == 3,
            SeriesKind::Price => mode == AdjustMode::All || e.flag == 3,
        }).count()
    };

    let rows = obs.into_iter()
        .map(|(obs_date, raw)| AdjRow {
            obs_date, raw,
            adjusted: apply_chain(kind, mode, obs_date, raw, &events),
        })
        .collect();
    Ok(AdjSeries { rows, factors_used, unusable_factors: unusable })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate { s.parse().unwrap() }
    fn ev(date: &str, amount: f64, operator: i16, flag: i16) -> FactorEvent {
        FactorEvent { event_date: d(date), amount, operator, flag }
    }

    /// The P0-measured AAPL split: 2020-08-31, factor 4.0, operator 1 (div
    /// for prices), flag 3 (prices and volumes).
    #[test]
    fn aapl_split_divides_prices_and_multiplies_volumes() {
        let events = [ev("2020-08-31", 4.0, 1, 3)];
        for mode in [AdjustMode::Splits, AdjustMode::All] {
            assert_eq!(apply_chain(SeriesKind::Price, mode,
                                   d("2020-08-28"), 400.0, &events), 100.0);
            assert_eq!(apply_chain(SeriesKind::Volume, mode,
                                   d("2020-08-28"), 1000.0, &events), 4000.0);
        }
        // The adjustment date itself is NOT adjusted (strictly >).
        assert_eq!(apply_chain(SeriesKind::Price, AdjustMode::All,
                               d("2020-08-31"), 129.04, &events), 129.04);
    }

    /// The live RMS dividend factor: operator 2 (mult), flag 1 (prices only).
    #[test]
    fn dividend_factors_touch_prices_only_and_only_in_all_mode() {
        let events = [ev("2025-05-05", 0.994902, 2, 1)];
        let p = apply_chain(SeriesKind::Price, AdjustMode::All,
                            d("2025-04-30"), 2400.0, &events);
        assert!((p - 2400.0 * 0.994902).abs() < 1e-9);
        assert_eq!(apply_chain(SeriesKind::Price, AdjustMode::Splits,
                               d("2025-04-30"), 2400.0, &events), 2400.0,
                   "a cash dividend is not a split");
        assert_eq!(apply_chain(SeriesKind::Volume, AdjustMode::All,
                               d("2025-04-30"), 500.0, &events), 500.0,
                   "flag 1 means prices only, whatever the mode");
    }

    /// Live RMS evidence: ordinary + extraordinary dividend, one ex-date --
    /// two real events, both apply.
    #[test]
    fn twin_same_day_events_both_apply() {
        let events = [ev("2025-05-05", 0.994902, 2, 1),
                      ev("2025-05-05", 0.995901, 2, 1)];
        let p = apply_chain(SeriesKind::Price, AdjustMode::All,
                            d("2025-04-30"), 1000.0, &events);
        assert!((p - 1000.0 * 0.994902 * 0.995901).abs() < 1e-9);
    }

    /// (10 + 2) * 3 = 36, not 10 * 3 + 2 = 32: events compose in the order
    /// they happened.
    #[test]
    fn additive_operators_apply_in_chronological_order() {
        let events = [ev("2020-01-10", 2.0, 3, 3), ev("2020-06-10", 3.0, 2, 3)];
        assert_eq!(apply_chain(SeriesKind::Price, AdjustMode::All,
                               d("2019-12-31"), 10.0, &events), 36.0);
        // Volumes: opposite -- (10 - 2) / 3.
        let v = apply_chain(SeriesKind::Volume, AdjustMode::All,
                            d("2019-12-31"), 10.0, &events);
        assert!((v - (10.0 - 2.0) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn raw_mode_is_identity_and_series_kind_detects_volume() {
        let events = [ev("2020-08-31", 4.0, 1, 3)];
        assert_eq!(apply_chain(SeriesKind::Price, AdjustMode::Raw,
                               d("2020-08-28"), 400.0, &events), 400.0);
        assert_eq!(series_kind("PX_VOLUME"), SeriesKind::Volume);
        assert_eq!(series_kind("px_volume"), SeriesKind::Volume);
        assert_eq!(series_kind("PX_LAST"), SeriesKind::Price);
    }
}
