//! Scoring candidate securities against the hints the user supplied.
//!
//! Deliberately a rule, not a model: the user must be able to read why a
//! candidate won, and the same input must always give the same answer. Ties are
//! not broken -- see `verdict`.

use crate::resolution::normalize::is_option_contract;
use serde::{Deserialize, Serialize};

/// What the user told us beyond the identifier itself. All optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hints {
    pub exchange: Option<String>,
    pub country: Option<String>,
    pub currency: Option<String>,
    pub asset_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub security: String,
    pub description: String,
    pub exchange: Option<String>,
    pub country: Option<String>,
    pub currency: Option<String>,
    pub asset_class: Option<String>,
    pub figi: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scored {
    pub candidate: Candidate,
    pub score: i32,
    pub disqualified: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug)]
pub enum Verdict {
    /// Exactly one candidate survived with a strictly highest score.
    Unique(Candidate),
    /// Two or more survivors, or a tie at the top. A human decides.
    Ambiguous(Vec<Scored>),
    /// Nothing survived.
    None,
}

/// One hint dimension. Returns (points, disqualified, reason).
fn compare(name: &str, hint: Option<&String>, value: Option<&String>)
    -> (i32, bool, Option<String>)
{
    match (hint, value) {
        (None, _) => (0, false, None),
        // A hint the user typed into and then cleared arrives as Some("") from a
        // UI text field, not None. Treating it as a real hint would disqualify
        // every candidate that says anything at all -- so a blank hint is no hint.
        (Some(h), _) if h.trim().is_empty() => (0, false, None),
        // The candidate is silent: absence of evidence is not evidence.
        (Some(_), None) => (0, false, Some(format!("{name}: candidate is silent"))),
        (Some(h), Some(v)) if h.trim().eq_ignore_ascii_case(v.trim()) => {
            (1, false, Some(format!("{name}: matches {h}")))
        }
        (Some(h), Some(v)) => (0, true, Some(format!("{name}: {v} contradicts {h}"))),
    }
}

pub fn score_all(candidates: Vec<Candidate>, hints: &Hints) -> Vec<Scored> {
    candidates
        .into_iter()
        .filter(|c| !is_option_contract(&c.security))
        .map(|c| {
            let dims = [
                compare("exchange", hints.exchange.as_ref(), c.exchange.as_ref()),
                compare("country", hints.country.as_ref(), c.country.as_ref()),
                compare("currency", hints.currency.as_ref(), c.currency.as_ref()),
                compare("asset_class", hints.asset_class.as_ref(), c.asset_class.as_ref()),
            ];
            let score = dims.iter().map(|(p, _, _)| p).sum();
            let disqualified = dims.iter().any(|(_, d, _)| *d);
            let reasons = dims.iter().filter_map(|(_, _, r)| r.clone()).collect();
            Scored { candidate: c, score, disqualified, reasons }
        })
        .collect()
}

/// Ambiguity is the default, not the exception. A candidate is only bound when
/// it is the sole survivor or beats every other survivor outright.
pub fn verdict(scored: Vec<Scored>) -> Verdict {
    let mut live: Vec<Scored> = scored.into_iter().filter(|s| !s.disqualified).collect();
    if live.is_empty() {
        return Verdict::None;
    }
    if live.len() == 1 {
        return Verdict::Unique(live.remove(0).candidate);
    }
    live.sort_by(|a, b| b.score.cmp(&a.score));
    if live[0].score > live[1].score {
        return Verdict::Unique(live.remove(0).candidate);
    }
    Verdict::Ambiguous(live)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(sec: &str, exch: &str, ccy: &str) -> Candidate {
        Candidate {
            security: sec.into(),
            description: String::new(),
            exchange: (!exch.is_empty()).then(|| exch.to_string()),
            country: None,
            currency: (!ccy.is_empty()).then(|| ccy.to_string()),
            asset_class: None,
            figi: None,
        }
    }

    fn hints(exch: Option<&str>, ccy: Option<&str>) -> Hints {
        Hints { exchange: exch.map(str::to_string), country: None,
                currency: ccy.map(str::to_string), asset_class: None }
    }

    #[test]
    fn with_no_hints_a_single_candidate_wins() {
        let v = verdict(score_all(vec![cand("AAPL US Equity", "US", "USD")],
                                  &hints(None, None)));
        assert!(matches!(v, Verdict::Unique(c) if c.security == "AAPL US Equity"));
    }

    #[test]
    fn with_no_hints_several_candidates_are_ambiguous() {
        let v = verdict(score_all(
            vec![cand("AAPL US Equity", "US", "USD"), cand("AAPL LN Equity", "LN", "GBP")],
            &hints(None, None)));
        assert!(matches!(v, Verdict::Ambiguous(ref s) if s.len() == 2),
                "no hint can separate them, so a human must");
    }

    #[test]
    fn a_matching_hint_selects_and_a_contradicting_one_disqualifies() {
        let scored = score_all(
            vec![cand("AAPL US Equity", "US", "USD"), cand("AAPL LN Equity", "LN", "GBP")],
            &hints(Some("US"), None));
        let us = scored.iter().find(|s| s.candidate.security.contains(" US ")).unwrap();
        let ln = scored.iter().find(|s| s.candidate.security.contains(" LN ")).unwrap();
        assert!(!us.disqualified && us.score > 0);
        assert!(ln.disqualified, "a candidate contradicting a supplied hint is out");
        assert!(matches!(verdict(scored), Verdict::Unique(c) if c.exchange.as_deref() == Some("US")));
    }

    #[test]
    fn a_candidate_silent_on_a_hint_is_neither_rewarded_nor_punished() {
        let quiet = Candidate { currency: None, ..cand("AAPL US Equity", "US", "") };
        let scored = score_all(vec![quiet], &hints(None, Some("USD")));
        assert!(!scored[0].disqualified);
        assert_eq!(scored[0].score, 0, "an absent attribute is not evidence either way");
    }

    #[test]
    fn hints_are_compared_case_insensitively() {
        let scored = score_all(vec![cand("AAPL US Equity", "us", "usd")],
                               &hints(Some("US"), Some("USD")));
        assert!(!scored[0].disqualified);
        assert!(scored[0].score >= 2, "both hints matched");
    }

    /// If the top two scores are equal the input genuinely does not distinguish
    /// them. Picking the first would be arbitrary and would silently bind the
    /// wrong instrument, which is the failure this whole phase exists to prevent.
    #[test]
    fn a_tie_at_the_top_is_ambiguous_not_a_coin_flip() {
        let v = verdict(score_all(
            vec![cand("AAPL US Equity", "US", "USD"), cand("AAPL UW Equity", "US", "USD")],
            &hints(Some("US"), Some("USD"))));
        match v {
            Verdict::Ambiguous(s) => assert_eq!(s.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn every_candidate_disqualified_is_not_ambiguity_but_absence() {
        let v = verdict(score_all(vec![cand("AAPL LN Equity", "LN", "GBP")],
                                  &hints(Some("US"), None)));
        assert!(matches!(v, Verdict::None));
    }

    /// P0 6: a query for AAPL returns option contracts alongside the listings.
    #[test]
    fn option_contracts_are_dropped_before_scoring() {
        let scored = score_all(
            vec![cand("AAPL US Equity", "US", "USD"),
                 cand("AAPL US 08/21/26 C400 Equity", "US", "USD")],
            &hints(None, None));
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].candidate.security, "AAPL US Equity");
    }

    /// A UI text field the user typed into and then cleared yields Some(""),
    /// not None. Before this was handled, clearing a box disqualified every
    /// candidate and the search went silently empty.
    #[test]
    fn a_blank_hint_is_no_hint() {
        let blank = Hints { exchange: Some("".into()), country: None,
                            currency: Some("   ".into()), asset_class: None };
        let scored = score_all(vec![cand("AAPL US Equity", "US", "USD")], &blank);
        assert!(!scored[0].disqualified, "a cleared field must not disqualify anything");
        assert_eq!(scored[0].score, 0, "and must not score either");
        assert!(scored[0].reasons.is_empty(), "nothing was asked, so nothing to explain");
        assert!(matches!(verdict(scored), Verdict::Unique(_)));
    }
}
