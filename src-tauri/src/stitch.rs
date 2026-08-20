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
}

#[derive(Debug, PartialEq)]
pub struct Junction {
    pub predecessor_id: i64,
    pub effective_date: NaiveDate,
    pub link_type: String,
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
        });
        if !seen.insert(link.predecessor_id) {
            return (junctions, ChainStop::Cycle);
        }
        current = link.predecessor_id;
        before = Some(link.effective_date);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> NaiveDate { s.parse().unwrap() }
    fn link(p: i64, s: i64, ty: &str, date: &str) -> LinkRow {
        LinkRow { predecessor_id: p, successor_id: s,
                  link_type: ty.into(), effective_date: d(date) }
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
                       link_type: "rename".into() },
            Junction { predecessor_id: 1, effective_date: d("2020-01-12"),
                       link_type: "merger".into() },
        ]);
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
