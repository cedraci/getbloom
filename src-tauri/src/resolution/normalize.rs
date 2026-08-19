//! Turning what a human (or Bloomberg) typed into a security string the
//! Terminal will accept. No I/O: everything here is a pure function, because
//! every one of these rules is worth a test and none of them needs a database.

use crate::error::{AppError, AppResult};

/// Bloomberg market sectors. The list is closed; an identifier ending in one of
/// these already carries its yellow key.
pub const YELLOW_KEYS: [&str; 9] = [
    "Equity", "Corp", "Govt", "Index", "Curncy", "Comdty", "Mtge", "Muni", "Pfd",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Ticker,
    Isin,
}

/// An ISIN is a 2-letter country code, 9 alphanumerics and a check digit.
/// Anything else is treated as a ticker; the user can override in the UI.
pub fn detect_id_kind(input: &str) -> IdKind {
    let s = input.trim();
    let s = s.strip_prefix("/isin/").unwrap_or(s);
    let s = strip_trailing_key_any(s);
    let bytes = s.as_bytes();
    let looks_like_isin = bytes.len() == 12
        && bytes[..2].iter().all(|b| b.is_ascii_alphabetic())
        && bytes[2..].iter().all(|b| b.is_ascii_alphanumeric());
    if looks_like_isin { IdKind::Isin } else { IdKind::Ticker }
}

/// Drop a yellow key the user already typed onto the identifier.
///
/// The obvious thing to paste into a "ticker" box is the whole Bloomberg
/// identifier, "AAPL US Equity" -- while the yellow-key box next to it already
/// says "Equity". Appending blindly produced "AAPL US Equity Equity", which the
/// Terminal rejects as INVALID_SECURITY. No real ticker ends in a
/// whitespace-separated yellow key, so stripping one is unambiguous.
fn strip_trailing_key(identifier: &str, yellow_key: &str) -> String {
    match identifier.rsplit_once(char::is_whitespace) {
        Some((head, tail))
            if tail.eq_ignore_ascii_case(yellow_key) && !head.trim().is_empty() =>
        {
            head.trim_end().to_string()
        }
        _ => identifier.to_string(),
    }
}

/// Same, but for any known yellow key -- used when detecting the id kind, where
/// the intended key is not yet known.
fn strip_trailing_key_any(identifier: &str) -> &str {
    if let Some((head, tail)) = identifier.rsplit_once(char::is_whitespace) {
        if YELLOW_KEYS.iter().any(|k| tail.eq_ignore_ascii_case(k))
            && !head.trim().is_empty()
        {
            return head.trim_end();
        }
    }
    identifier
}

pub fn build_security(kind: IdKind, identifier: &str, yellow_key: &str)
    -> AppResult<String>
{
    let yk = yellow_key.trim();
    if yk.is_empty() {
        return Err(AppError::Validation("yellow_key is required".into()));
    }
    let raw = identifier.trim();
    if raw.is_empty() {
        return Err(AppError::Validation("identifier is empty".into()));
    }
    match kind {
        IdKind::Ticker => {
            let t = strip_trailing_key(raw, yk);
            // A ticker that IS the yellow key never had a security in it;
            // stripping cannot help, so refuse rather than build "Equity Equity".
            if t.is_empty() || t.eq_ignore_ascii_case(yk) {
                return Err(AppError::Validation(
                    "identifier is only a yellow key -- enter the security, e.g. 'AAPL US'".into()));
            }
            Ok(format!("{t} {yk}"))
        }
        IdKind::Isin => {
            // Accept a pasted "/isin/FR0000120271 Corp" as readily as a bare ISIN.
            let i = strip_trailing_key(raw, yk);
            let i = i.strip_prefix("/isin/").unwrap_or(&i).trim();
            if i.is_empty() {
                return Err(AppError::Validation("isin is empty after normalisation".into()));
            }
            Ok(format!("/isin/{i} {yk}"))
        }
    }
}

/// Convert Bloomberg's instrumentListRequest form into a security string.
///
/// P0 6 observed the service returns "AAPL US<equity>". The Terminal does not
/// accept that form, so it is normalised the moment it arrives and the raw text
/// is kept only for display. Returns None when the trailing key is not a known
/// market sector: a candidate we cannot address is worse than no candidate.
pub fn normalize_bbg_security(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let Some((head, rest)) = s.rsplit_once('<') else {
        // Already in "AAPL US Equity" form: accept it if its key is known.
        let tail = s.rsplit_once(char::is_whitespace).map(|(_, t)| t)?;
        return YELLOW_KEYS.iter().any(|k| tail.eq_ignore_ascii_case(k))
            .then(|| s.to_string());
    };
    let key = rest.strip_suffix('>')?;
    let canonical = YELLOW_KEYS.iter().find(|k| k.eq_ignore_ascii_case(key))?;
    let head = head.trim();
    (!head.is_empty()).then(|| format!("{head} {canonical}"))
}

/// A listed option carries an expiry date and a strike between the ticker and
/// the yellow key: "AAPL US 08/21/26 C400 Equity". These are excluded from
/// candidate sets -- they are not instruments the security master tracks, and
/// they make every equity search ambiguous.
pub fn is_option_contract(security: &str) -> bool {
    let mut parts = security.split_whitespace().peekable();
    let mut saw_date = false;
    while let Some(p) = parts.next() {
        // MM/DD/YY or MM/DD/YYYY
        if p.matches('/').count() == 2
            && p.split('/').all(|seg| !seg.is_empty()
                                 && seg.chars().all(|c| c.is_ascii_digit()))
        {
            saw_date = true;
            continue;
        }
        // A call or put strike immediately usable after the date: C400, P150.
        if saw_date {
            let mut cs = p.chars();
            if matches!(cs.next(), Some('C') | Some('P'))
                && p.len() > 1
                && cs.all(|c| c.is_ascii_digit() || c == '.')
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_and_yellow_key_are_joined() {
        assert_eq!(build_security(IdKind::Ticker, "AAPL US", "Equity").unwrap(),
                   "AAPL US Equity");
    }

    #[test]
    fn isin_gets_the_slash_isin_form() {
        assert_eq!(build_security(IdKind::Isin, "FR0000120271", "Corp").unwrap(),
                   "/isin/FR0000120271 Corp");
        for input in ["FR0000120271", "/isin/FR0000120271", "/isin/FR0000120271 Corp"] {
            assert_eq!(build_security(IdKind::Isin, input, "Corp").unwrap(),
                       "/isin/FR0000120271 Corp", "input {input:?}");
        }
    }

    /// Regression, carried over from migration 0004 and registry.rs. Runs 1 and 2
    /// on 2026-08-17 both asked Bloomberg for "AAPL US Equity Equity" and were
    /// rejected with BAD_SEC/INVALID_SECURITY. The ticker looked perfectly valid
    /// in the UI, because the duplication existed only in the derived security.
    #[test]
    fn a_ticker_carrying_its_own_yellow_key_is_not_doubled() {
        for input in ["AAPL US Equity", "AAPL US equity", "AAPL US EQUITY",
                      "  AAPL US Equity  "] {
            assert_eq!(build_security(IdKind::Ticker, input, "Equity").unwrap(),
                       "AAPL US Equity", "input {input:?}");
        }
        // A different key is not a duplicate and must survive untouched.
        assert_eq!(build_security(IdKind::Ticker, "AAPL US Equity", "Corp").unwrap(),
                   "AAPL US Equity Corp");
        // Nothing left once the key is stripped is a user error, not a silent pass.
        assert!(build_security(IdKind::Ticker, "Equity", "Equity").is_err());
    }

    #[test]
    fn inputs_are_trimmed_and_the_key_is_required() {
        assert_eq!(build_security(IdKind::Ticker, " AAPL US ", " Equity ").unwrap(),
                   "AAPL US Equity");
        assert!(build_security(IdKind::Ticker, "AAPL US", "  ").is_err());
        assert!(build_security(IdKind::Ticker, "", "Equity").is_err());
    }

    /// An ISIN is two letters, nine alphanumerics and a check digit. Anything
    /// else the user types is a ticker, including tickers that begin with two
    /// letters.
    #[test]
    fn id_kind_is_detected_from_the_shape_of_the_input() {
        assert_eq!(detect_id_kind("FR0000120271"), IdKind::Isin);
        assert_eq!(detect_id_kind("us0378331005"), IdKind::Isin);
        assert_eq!(detect_id_kind("/isin/FR0000120271"), IdKind::Isin);
        assert_eq!(detect_id_kind("AAPL US"), IdKind::Ticker);
        assert_eq!(detect_id_kind("FR"), IdKind::Ticker);
        assert_eq!(detect_id_kind("FR0000120271X"), IdKind::Ticker);  // too long
    }

    /// P0 6: instrumentListRequest returns "AAPL US<equity>", which the Terminal
    /// does NOT accept as a security. Pasting it produces exactly the malformed
    /// identifier migration 0004 had to repair, so it is normalised on arrival
    /// and the raw form is never used as a security string.
    #[test]
    fn bloomberg_list_output_is_normalised_to_a_security_string() {
        assert_eq!(normalize_bbg_security("AAPL US<equity>").as_deref(),
                   Some("AAPL US Equity"));
        assert_eq!(normalize_bbg_security("T 4 ⅜ 05/15/41<govt>").as_deref(),
                   Some("T 4 ⅜ 05/15/41 Govt"));
        assert_eq!(normalize_bbg_security("VFIAX US<equity>").as_deref(),
                   Some("VFIAX US Equity"));
        // Already-normal input passes through unchanged.
        assert_eq!(normalize_bbg_security("AAPL US Equity").as_deref(),
                   Some("AAPL US Equity"));
        // An unknown key is not silently accepted -- better no candidate than a
        // candidate that will come back BAD_SEC.
        assert_eq!(normalize_bbg_security("AAPL US<nonsense>"), None);
        assert_eq!(normalize_bbg_security(""), None);
    }

    /// P0 6: a query for "AAPL" returns option contracts alongside the listings.
    /// They are not instruments the security master tracks, and including them
    /// makes every equity search ambiguous.
    #[test]
    fn option_contracts_are_recognised() {
        assert!(is_option_contract("AAPL US 08/21/26 C400 Equity"));
        assert!(is_option_contract("AAPL US 08/21/26 P150 Equity"));
        assert!(is_option_contract("AAPL US 12/19/25 C00220000 Equity"));
        assert!(!is_option_contract("AAPL US Equity"));
        assert!(!is_option_contract("VFIAX US Equity"));
        assert!(!is_option_contract("SX5E Index"));
    }
}
