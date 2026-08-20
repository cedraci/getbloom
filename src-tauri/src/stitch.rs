//! P5: merger stitching (design:
//! docs/superpowers/specs/2026-08-21-p5-merger-stitching-design.md).
//!
//! Extends a surviving instrument's series backward through CONFIRMED
//! links, spliced in successor units. Derived on read, like P4: nothing is
//! stored, and the human confirmation gate on links stands (P0 7.2).

use chrono::NaiveDate;

/// A confirmed link row, as loaded by the caller. Spinoffs may be present
/// in the slice; the planner ignores them (a child's history is not the
/// parent's).
#[derive(Debug, Clone)]
pub struct LinkRow {
    pub predecessor_id: i64,
    pub successor_id: i64,
    pub link_type: String,
    pub effective_date: NaiveDate,
    /// P6: Bloomberg's asserted exchange ratio (ACQUIRER shares per TARGET
    /// share, from CA_MA_STOCK_TERMS), when the link carries one.
    pub exchange_ratio: Option<f64>,
}

#[derive(Debug, PartialEq)]
pub struct Junction {
    pub predecessor_id: i64,
    pub effective_date: NaiveDate,
    pub link_type: String,
    pub exchange_ratio: Option<f64>,
}

#[derive(Debug, PartialEq)]
pub enum ChainStop {
    /// No further confirmed link: the natural start of the history.
    End,
    /// Two links tie on the junction date: picking one would fabricate
    /// history, so the walk stops and says so.
    Ambiguous(NaiveDate),
    Cycle,
    DepthCap,
}

const DEPTH_CAP: usize = 10;

/// Walk backward from `target`: at each step, among links whose successor
/// is the current instrument and whose effective_date is strictly before
/// the previous junction's date, take the latest.
pub fn plan_chain(target: i64, links: &[LinkRow]) -> (Vec<Junction>, ChainStop) {
    let mut junctions: Vec<Junction> = Vec::new();
    let mut seen = std::collections::HashSet::from([target]);
    let mut current = target;
    let mut before: Option<NaiveDate> = None;
    loop {
        if junctions.len() >= DEPTH_CAP {
            return (junctions, ChainStop::DepthCap);
        }
        let candidates: Vec<&LinkRow> = links.iter()
            .filter(|l| l.successor_id == current
                && l.link_type != "spinoff"
                && before.is_none_or(|b| l.effective_date < b))
            .collect();
        let Some(latest) = candidates.iter().map(|l| l.effective_date).max() else {
            return (junctions, ChainStop::End);
        };
        let at_latest: Vec<&&LinkRow> = candidates.iter()
            .filter(|l| l.effective_date == latest).collect();
        if at_latest.len() > 1 {
            return (junctions, ChainStop::Ambiguous(latest));
        }
        let link = at_latest[0];
        junctions.push(Junction {
            predecessor_id: link.predecessor_id,
            effective_date: link.effective_date,
            link_type: link.link_type.clone(),
            exchange_ratio: link.exchange_ratio,
        });
        if !seen.insert(link.predecessor_id) {
            return (junctions, ChainStop::Cycle);
        }
        current = link.predecessor_id;
        before = Some(link.effective_date);
    }
}

// ------------------------------------------------------------- composition

#[derive(Debug, serde::Serialize)]
pub struct StitchRow {
    pub obs_date: NaiveDate,
    pub value: f64,
    pub source_instrument_id: i64,
}

#[derive(Debug, serde::Serialize)]
pub struct SegmentInfo {
    pub instrument_id: i64,
    pub label: Option<String>,
    pub from: Option<NaiveDate>,
    pub to: Option<NaiveDate>,
    /// The link that attached this segment (None for the queried instrument).
    pub link_type: Option<String>,
    /// The junction ratio applied to reach successor units (not cumulative).
    pub ratio: Option<f64>,
    pub note: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub struct StitchedSeries {
    /// obs_date DESC, values in the QUERIED instrument's units.
    pub rows: Vec<StitchRow>,
    pub segments: Vec<SegmentInfo>,
    /// Why the backward walk ended early, when it did not end naturally.
    pub stopped: Option<String>,
}

pub async fn has_confirmed_predecessors(pool: &sqlx::PgPool, instrument_id: i64)
    -> crate::error::AppResult<bool>
{
    Ok(sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM instrument_link
                         WHERE successor_id = $1 AND confirmed_by IS NOT NULL
                           AND link_type <> 'spinoff')")
        .bind(instrument_id).fetch_one(pool).await?)
}

async fn segment_label(pool: &sqlx::PgPool, instrument_id: i64)
    -> crate::error::AppResult<Option<String>>
{
    Ok(sqlx::query_scalar(
        "SELECT label FROM book_entry WHERE instrument_id = $1")
        .bind(instrument_id).fetch_optional(pool).await?)
}

/// The queried instrument's series, extended backward through confirmed
/// links (design 3): per segment the P4-adjusted series in `mode`, spliced
/// at each junction by the derived ratio. Read-only, derived, never stored.
pub async fn stitched_series(pool: &sqlx::PgPool, instrument_id: i64,
                             field_id: i64, mode: crate::adjust::AdjustMode,
                             limit: i64)
    -> crate::error::AppResult<StitchedSeries>
{
    let links: Vec<(i64, i64, String, NaiveDate, Option<f64>)> = sqlx::query_as(
        "SELECT predecessor_id, successor_id, link_type, effective_date,
                exchange_ratio
           FROM instrument_link WHERE confirmed_by IS NOT NULL")
        .fetch_all(pool).await?;
    let links: Vec<LinkRow> = links.into_iter()
        .map(|(predecessor_id, successor_id, link_type, effective_date,
               exchange_ratio)| LinkRow {
            predecessor_id, successor_id, link_type, effective_date,
            exchange_ratio })
        .collect();
    let (junctions, stop) = plan_chain(instrument_id, &links);
    let mut stopped = match stop {
        ChainStop::End => None,
        ChainStop::Ambiguous(d) => Some(format!(
            "two confirmed links share the junction date {d}; extend stops \
             there rather than pick a history")),
        ChainStop::Cycle => Some("the link chain loops back on itself".into()),
        ChainStop::DepthCap => Some("the link chain exceeds 10 junctions".into()),
    };

    let is_volume = {
        let mnemonic: String = sqlx::query_scalar(
            "SELECT mnemonic FROM field_def WHERE id = $1")
            .bind(field_id).fetch_one(pool).await?;
        crate::adjust::series_kind(&mnemonic) == crate::adjust::SeriesKind::Volume
    };

    let own = crate::adjust::adjusted_series(
        pool, instrument_id, field_id, mode, 5000).await?;
    let first_junction = junctions.first().map(|j| j.effective_date);
    let mut rows: Vec<StitchRow> = own.rows.iter()
        .filter(|r| first_junction.is_none_or(|d| r.obs_date >= d))
        .map(|r| StitchRow { obs_date: r.obs_date, value: r.adjusted,
                             source_instrument_id: instrument_id })
        .collect();
    let mut segments = vec![SegmentInfo {
        instrument_id,
        label: segment_label(pool, instrument_id).await?,
        from: first_junction,
        to: None,
        link_type: None, ratio: None, note: None,
    }];

    // `prev` = the segment on the successor side of the next junction, in
    // its OWN units (the cumulative factor converts to queried units).
    let mut prev = own;
    let mut cumulative = 1.0f64;
    for (k, j) in junctions.iter().enumerate() {
        let d = j.effective_date;
        let pred = crate::adjust::adjusted_series(
            pool, j.predecessor_id, field_id, mode, 5000).await?;
        let window_start = junctions.get(k + 1).map(|n| n.effective_date);
        let mut ratio_note: Option<String> = None;
        let ratio = if is_volume {
            1.0
        } else if j.link_type == "rename" || j.link_type == "share_class_change" {
            1.0
        } else if let Some(r) = j.exchange_ratio.filter(|r| *r > 0.0) {
            // P6: Bloomberg asserted the terms (CA_MA_STOCK_TERMS, r =
            // acquirer shares per target share). One target share became r
            // successor shares, so a target price divides by r to land in
            // successor units -- pinned live: XLNX 194.92 / 1.7234 = 113.1
            // against AMD's 114.27 first close.
            ratio_note = Some(format!(
                "splice from Bloomberg terms: {r} acquirer sh. per target sh."));
            1.0 / r
        } else {
            // rows are obs_date DESC; junction values sit inside each
            // segment's own window by construction.
            let succ_val = prev.rows.iter().rev()
                .find(|r| r.obs_date >= d).map(|r| r.adjusted);
            let pred_val = pred.rows.iter()
                .find(|r| r.obs_date < d).map(|r| r.adjusted);
            match (succ_val, pred_val) {
                (Some(s), Some(p)) if p != 0.0 => s / p,
                _ => {
                    stopped = Some(format!(
                        "no junction ratio at {d}: both sides need an \
                         observation around the effective date"));
                    break;
                }
            }
        };
        cumulative *= ratio;
        let seg_rows: Vec<StitchRow> = pred.rows.iter()
            .filter(|r| r.obs_date < d
                && window_start.is_none_or(|w| r.obs_date >= w))
            .map(|r| StitchRow { obs_date: r.obs_date,
                                 value: r.adjusted * cumulative,
                                 source_instrument_id: j.predecessor_id })
            .collect();
        segments.push(SegmentInfo {
            instrument_id: j.predecessor_id,
            label: segment_label(pool, j.predecessor_id).await?,
            from: window_start,
            to: Some(d),
            link_type: Some(j.link_type.clone()),
            ratio: Some(ratio),
            note: if is_volume && j.link_type != "rename"
                     && j.link_type != "share_class_change" {
                Some("volumes concatenated unscaled".into())
            } else { ratio_note },
        });
        rows.extend(seg_rows);
        prev = pred;
    }

    rows.truncate(limit.clamp(1, 5000) as usize);
    Ok(StitchedSeries { rows, segments, stopped })
}

/// Full-depth stitched series to CSV; returns rows written.
pub async fn export_stitched_csv(pool: &sqlx::PgPool, instrument_id: i64,
                                 field_id: i64, mode: crate::adjust::AdjustMode,
                                 path: &std::path::Path)
    -> crate::error::AppResult<u64>
{
    let s = stitched_series(pool, instrument_id, field_id, mode, 5000).await?;
    let mut out = String::from("obs_date,value,source_instrument_id\n");
    for r in &s.rows {
        out.push_str(&crate::dataview::csv_line(&[
            r.obs_date.to_string(),
            r.value.to_string(),
            r.source_instrument_id.to_string(),
        ]));
        out.push('\n');
    }
    std::fs::write(path, out)?;
    Ok(s.rows.len() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate { s.parse().unwrap() }
    fn link(p: i64, s: i64, ty: &str, date: &str) -> LinkRow {
        LinkRow { predecessor_id: p, successor_id: s,
                  link_type: ty.into(), effective_date: d(date),
                  exchange_ratio: None }
    }

    #[test]
    fn a_straight_chain_walks_backward_with_descending_dates() {
        // A(1) merged into B(2) on 2020-01-12; B renamed to C(3) on 2024-06-01.
        let links = [link(1, 2, "merger", "2020-01-12"),
                     link(2, 3, "rename", "2024-06-01")];
        let (junctions, stop) = plan_chain(3, &links);
        assert_eq!(stop, ChainStop::End);
        assert_eq!(junctions, vec![
            Junction { predecessor_id: 2, effective_date: d("2024-06-01"),
                       link_type: "rename".into(), exchange_ratio: None },
            Junction { predecessor_id: 1, effective_date: d("2020-01-12"),
                       link_type: "merger".into(), exchange_ratio: None },
        ]);
    }

    /// P6: the asserted ratio rides the junction so the composer can prefer
    /// it over price-continuity derivation.
    #[test]
    fn an_asserted_exchange_ratio_travels_with_its_junction() {
        let mut l = link(1, 2, "merger", "2022-02-15");
        l.exchange_ratio = Some(1.7234);
        let (junctions, stop) = plan_chain(2, &[l]);
        assert_eq!(stop, ChainStop::End);
        assert_eq!(junctions[0].exchange_ratio, Some(1.7234));
    }

    #[test]
    fn spinoff_links_are_never_followed() {
        let links = [link(1, 2, "spinoff", "2020-01-12")];
        let (junctions, stop) = plan_chain(2, &links);
        assert!(junctions.is_empty());
        assert_eq!(stop, ChainStop::End);
    }

    #[test]
    fn a_tie_on_the_junction_date_stops_the_walk_as_ambiguous() {
        // Two funds absorbed the same day: which history continues? Neither,
        // without a human answer.
        let links = [link(1, 3, "merger", "2020-01-12"),
                     link(2, 3, "merger", "2020-01-12")];
        let (junctions, stop) = plan_chain(3, &links);
        assert!(junctions.is_empty());
        assert_eq!(stop, ChainStop::Ambiguous(d("2020-01-12")));
    }

    #[test]
    fn a_cycle_is_detected() {
        let links = [link(1, 2, "merger", "2020-01-12"),
                     link(2, 1, "merger", "2019-01-12")];
        let (junctions, stop) = plan_chain(2, &links);
        assert_eq!(junctions.len(), 2, "1 then 2-again is caught at re-entry");
        assert_eq!(stop, ChainStop::Cycle);
    }

    #[test]
    fn junction_dates_must_strictly_descend() {
        // The A->B link is dated AFTER the B->C junction: inside a range C's
        // own history already covers. Not a candidate.
        let links = [link(1, 2, "merger", "2025-03-03"),
                     link(2, 3, "rename", "2024-06-01")];
        let (junctions, stop) = plan_chain(3, &links);
        assert_eq!(junctions.len(), 1, "only the rename junction");
        assert_eq!(stop, ChainStop::End);
    }
}
