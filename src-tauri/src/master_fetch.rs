//! Bloomberg requests that serve the security master rather than the time
//! series: who is this security, what identifiers has it worn, and what else
//! matches this text.
//!
//! Every field name below is confirmed in the P0 fact sheet. Do not add one
//! that is not: six plausible-looking mnemonics were already proven not to
//! exist, and Bloomberg reports an unknown field as a per-field exception
//! rather than an error, so a guess degrades quietly into missing data.

use crate::error::{AppError, AppResult};
use crate::orchestrator::PipelineConfig;
use crate::resolution::normalize::normalize_bbg_security;
use crate::resolution::score::Candidate;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// The identity block requested at resolution step 3. All P0-verified (§6.1).
///
/// SIMP_SEC_STATUS is deliberately NOT here. It looked like a lifecycle field
/// and is not: P0 §10.2 measured it returning PREO, CLOS and HALT -- the market
/// session, updating in realtime. Requesting it would spend a call on a value
/// that is stale on arrival and meaningless to store. INACTIVE_DATE, below,
/// answers the question it was recruited for, with a date instead of a mood.
pub const IDENTITY_FIELDS: [&str; 12] = [
    "ID_BB_GLOBAL",
    "ID_BB_GLOBAL_SHARE_CLASS_LEVEL",
    "ID_BB_UNIQUE",
    "ID_ISIN",
    "EXCH_CODE",
    "CRNCY",
    "CNTRY_ISSUE_ISO",
    "SECURITY_TYP2",
    "MARKET_SECTOR_DES",
    "NAME",
    "LISTING_DATE",
    "INACTIVE_DATE",
];

/// What one identity request is charged to `hit_ledger`.
///
/// One hit per security-field pair, which is how `budget::estimate_eod_hits`
/// counts a reference request and what the whole estimator is calibrated
/// against (the Excel add-in's accounting). Whether the Desktop API meters it
/// identically is not established (P0 §10.5), so the project's standing
/// over-count-is-safe policy applies.
///
/// Public and separate from the request itself so the accounting can be
/// pinned by a test without a Terminal -- the number is a promise about the
/// budget screen, not an implementation detail.
pub fn identity_hit_cost(securities: usize) -> i64 {
    (securities * IDENTITY_FIELDS.len()) as i64
}

/// One security, one (bulk) field.
pub const HIST_IDS_HIT_COST: i64 = 1;

pub const HIST_IDS_FIELD: &str = "HISTORICAL_IDS_TIME_RANGE";
/// Overrides on HIST_IDS_FIELD, resolved from its own FieldInfoRequest (P0 §6.3).
pub const HIST_IDS_ANCHOR: &str = "HISTORICAL_STARTING_IDENTIFIER";
pub const HIST_IDS_START: &str = "HISTORICAL_ID_TM_RANGE_START_DT";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityBlock {
    pub security: String,
    pub figi: Option<String>,
    pub share_class_figi: Option<String>,
    pub bbg_unique: Option<String>,
    pub isin: Option<String>,
    pub exch_code: Option<String>,
    pub currency: Option<String>,
    pub country: Option<String>,
    pub security_typ2: Option<String>,
    pub market_sector: Option<String>,
    pub name: Option<String>,
    pub listing_date: Option<NaiveDate>,
    pub inactive_date: Option<NaiveDate>,
    /// Reserved for a lifecycle status P3/P5 may derive. Never populated from
    /// SIMP_SEC_STATUS -- see IDENTITY_FIELDS.
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistIdRow {
    pub date: NaiveDate,
    pub old_id: String,
    pub new_id: String,
    pub old_exch: Option<String>,
    pub new_exch: Option<String>,
    pub action_id: Option<String>,
    pub source: Option<String>,
}

/// A parsed response paired with the wire JSON it came from.
///
/// The parsed form is what callers use; `raw` is what gets written to
/// resolution_decision.bbg_response. An IdentityBlock is a lossy projection --
/// it drops fieldExceptions (including entitlement failures), securityError
/// detail, and every field we did not ask for -- so the audit trail must keep
/// the response itself, not our reading of it.
#[derive(Debug, Clone)]
pub struct Answered<T> {
    pub parsed: T,
    pub raw: serde_json::Value,
}

pub trait MasterFetcher {
    fn identity(&self, securities: &[String])
        -> impl std::future::Future<Output = AppResult<Answered<Vec<IdentityBlock>>>> + Send;

    /// `anchor` is mandatory, not optional. P0 §6.4: without
    /// HISTORICAL_STARTING_IDENTIFIER the answer may describe a different
    /// company that once wore the same ticker.
    fn hist_ids(&self, security: &str, anchor: &str, start: NaiveDate)
        -> impl std::future::Future<Output = AppResult<Vec<HistIdRow>>> + Send;

    fn instrument_list(&self, query: &str, yellow_key_filter: Option<&str>,
                       max_results: u32)
        -> impl std::future::Future<Output = AppResult<Answered<Vec<Candidate>>>> + Send;
}

// ------------------------------------------------------------------ parsing

fn s(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(str::to_string).filter(|t| !t.trim().is_empty())
}

fn date(v: &serde_json::Value) -> Option<NaiveDate> {
    v.as_str()?.parse().ok()
}

/// Walk the securityData array of every message in a response.
fn each_security<'a>(raw: &'a serde_json::Value)
    -> impl Iterator<Item = &'a serde_json::Value>
{
    raw.as_array().map(|v| v.as_slice()).unwrap_or(&[])
        .iter()
        .filter_map(|msg| msg.get("securityData"))
        .filter_map(|sd| sd.as_array())
        .flatten()
}

pub fn parse_identity(raw: &serde_json::Value) -> Vec<IdentityBlock> {
    each_security(raw)
        // A rejected security must not become a half-populated instrument.
        .filter(|sd| sd.get("securityError").is_none())
        .map(|sd| {
            let f = sd.get("fieldData").cloned().unwrap_or(serde_json::json!({}));
            let g = |k: &str| f.get(k).cloned().unwrap_or(serde_json::Value::Null);
            IdentityBlock {
                security: s(&sd["security"]).unwrap_or_default(),
                figi: s(&g("ID_BB_GLOBAL")),
                share_class_figi: s(&g("ID_BB_GLOBAL_SHARE_CLASS_LEVEL")),
                bbg_unique: s(&g("ID_BB_UNIQUE")),
                isin: s(&g("ID_ISIN")),
                exch_code: s(&g("EXCH_CODE")),
                currency: s(&g("CRNCY")),
                country: s(&g("CNTRY_ISSUE_ISO")),
                security_typ2: s(&g("SECURITY_TYP2")),
                market_sector: s(&g("MARKET_SECTOR_DES")),
                name: s(&g("NAME")),
                listing_date: date(&g("LISTING_DATE")),
                inactive_date: date(&g("INACTIVE_DATE")),
                status: None,
            }
        })
        .collect()
}

/// HISTORICAL_IDS_TIME_RANGE is a bulk field: its value is a list of dicts whose
/// column names are Bloomberg's own, spaces and all (P0 §6.3).
pub fn parse_hist_ids(raw: &serde_json::Value) -> Vec<HistIdRow> {
    each_security(raw)
        .filter(|sd| sd.get("securityError").is_none())
        .filter_map(|sd| sd.pointer(&format!("/fieldData/{HIST_IDS_FIELD}")))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(|row| {
            Some(HistIdRow {
                date: date(row.get("Date")?)?,
                old_id: s(row.get("Old ID")?)?,
                new_id: s(row.get("New ID")?)?,
                old_exch: row.get("Old Exch").and_then(s),
                new_exch: row.get("New Exch").and_then(s),
                action_id: row.get("Action ID").and_then(s),
                source: row.get("Source").and_then(s),
            })
        })
        .collect()
}

/// instrumentListRequest results. The `AAPL US<equity>` form is converted here,
/// on arrival; a candidate whose key is unrecognised is dropped rather than
/// carried forward as an identifier the Terminal will reject.
pub fn parse_list(raw: &serde_json::Value) -> Vec<Candidate> {
    raw.as_array().map(|v| v.as_slice()).unwrap_or(&[])
        .iter()
        .filter_map(|msg| msg.get("results"))
        .filter_map(|r| r.as_array())
        .flatten()
        .filter_map(|r| {
            let security = normalize_bbg_security(r.get("security")?.as_str()?)?;
            // "AAPL US Equity" -> exchange "US". Two tokens plus a yellow key is
            // the shape; anything else leaves the exchange unknown, which the
            // scorer treats as silence rather than contradiction.
            let parts: Vec<&str> = security.split_whitespace().collect();
            let exchange = (parts.len() == 3).then(|| parts[1].to_string());
            Some(Candidate {
                security,
                description: r.get("description").and_then(|d| d.as_str())
                    .unwrap_or_default().to_string(),
                exchange,
                country: None,
                currency: None,
                asset_class: None,
                figi: None,
            })
        })
        .collect()
}

// ------------------------------------------------------------------ live

/// The live fetcher, and the only place in the crate where a security-master
/// request reaches the wire.
///
/// It carries a pool because THIS is where the hit ledger is written. Every
/// earlier attempt put `record_purpose_hits` at the call sites, and four call
/// sites -- the two identity calls in `resolution::engine`, the history call,
/// and `resolve_review`'s identity call -- were added without one, so a bulk
/// import of four hundred rows spent hundreds of unrecorded Bloomberg hits
/// while the budget screen read zero. A guard at the seam cannot be bypassed
/// by a future call site, because a call that does not pass through here does
/// not reach Bloomberg at all.
///
/// `MockMasterFetcher` deliberately does NOT record: it has no pool, so no
/// test's ledger assertions change and no test needs a database to run a
/// fetcher.
pub struct BlpapiMasterFetcher<'a> {
    pub cfg: &'a PipelineConfig,
    pub pool: &'a sqlx::PgPool,
}

impl BlpapiMasterFetcher<'_> {
    async fn call(&self, spec: serde_json::Value) -> AppResult<serde_json::Value> {
        crate::blp_driver::run_raw(
            &self.cfg.python_path,
            &self.cfg.script_path,
            &serde_json::json!({
                "run_id": 0,
                "timeout_s": self.cfg.request_timeout_s,
                "requests": [spec],
            }),
        ).await
    }

    /// Charge the ledger for a request that has already succeeded.
    ///
    /// Logged and swallowed, never propagated: the Bloomberg call has been
    /// made and paid for by the time this runs, so turning a ledger write
    /// failure into a `?` would throw away the candidates that call bought
    /// and leave the user to spend the hit again. An undercounted budget is
    /// the smaller of the two harms, and it is visible in the log.
    async fn charge(&self, purpose: &str, hits: i64) {
        if let Err(e) = crate::budget::record_purpose_hits(self.pool, purpose, hits).await {
            eprintln!("hit ledger write failed for {purpose} ({hits} hits): {e}");
        }
    }
}

impl MasterFetcher for BlpapiMasterFetcher<'_> {
    async fn identity(&self, securities: &[String]) -> AppResult<Answered<Vec<IdentityBlock>>> {
        let resp = self.call(serde_json::json!({
            "kind": "reference",
            "securities": securities,
            "fields": IDENTITY_FIELDS,
            "obs_date": chrono::Local::now().date_naive().to_string(),
            "raw": true,
        })).await?;
        self.charge("resolve_identity", identity_hit_cost(securities.len())).await;
        let raw = resp["raw_messages"].clone();
        let parsed = parse_identity(&raw);
        Ok(Answered { parsed, raw })
    }

    async fn hist_ids(&self, security: &str, anchor: &str, start: NaiveDate)
        -> AppResult<Vec<HistIdRow>>
    {
        if anchor.trim().is_empty() {
            return Err(AppError::Validation(
                "hist_ids requires an anchoring identifier (P0 6.4)".into()));
        }
        let resp = self.call(serde_json::json!({
            "kind": "bulk_reference",
            "securities": [security],
            "fields": [HIST_IDS_FIELD],
            "overrides": [
                {"fieldId": HIST_IDS_ANCHOR, "value": anchor},
                {"fieldId": HIST_IDS_START, "value": start.format("%Y%m%d").to_string()},
            ],
            "raw": true,
        })).await?;
        self.charge("resolve_history", HIST_IDS_HIT_COST).await;
        Ok(parse_hist_ids(&resp["raw_messages"]))
    }

    async fn instrument_list(&self, query: &str, yellow_key_filter: Option<&str>,
                             max_results: u32) -> AppResult<Answered<Vec<Candidate>>>
    {
        let resp = self.call(serde_json::json!({
            "kind": "instrument_list",
            "query": query,
            "yellow_key_filter": yellow_key_filter,
            "max_results": max_results,
            "raw": true,
        })).await?;
        // Whether instrumentListRequest is metered at all is still open
        // (P0 §10.5); it is charged anyway, at the same conservative rate the
        // Search Bloomberg button used to charge from its own call site.
        self.charge("search", crate::budget::SEARCH_HIT_COST).await;
        let raw = resp["raw_messages"].clone();
        let parsed = parse_list(&raw);
        Ok(Answered { parsed, raw })
    }
}

// ------------------------------------------------------------------ mock

/// Replays a committed capture. Every test above the transport uses this, so
/// the whole resolution path is exercised without a Terminal.
pub struct MockMasterFetcher {
    pub identity_raw: serde_json::Value,
    pub hist_ids_raw: serde_json::Value,
    pub list_raw: serde_json::Value,
    /// Every call recorded, so a test can assert Bloomberg was NOT called.
    pub calls: std::sync::Mutex<Vec<String>>,
}

impl Default for MockMasterFetcher {
    fn default() -> Self {
        Self {
            identity_raw: serde_json::json!([]),
            hist_ids_raw: serde_json::json!([]),
            list_raw: serde_json::json!([]),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl MockMasterFetcher {
    /// Takes a P0 capture file and uses its first value for every request kind;
    /// tests that need finer control set the fields directly.
    pub fn from_capture(json: &str) -> Self {
        let all: serde_json::Value = serde_json::from_str(json).expect("capture json");
        let first = all.as_object()
            .and_then(|m| m.values().next().cloned())
            .unwrap_or(serde_json::json!([]));
        Self { hist_ids_raw: first.clone(), identity_raw: first.clone(),
               list_raw: first, ..Default::default() }
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn record(&self, what: &str) {
        self.calls.lock().unwrap().push(what.to_string());
    }
}

impl MasterFetcher for MockMasterFetcher {
    async fn identity(&self, securities: &[String]) -> AppResult<Answered<Vec<IdentityBlock>>> {
        self.record(&format!("identity:{}", securities.join(",")));
        Ok(Answered { parsed: parse_identity(&self.identity_raw), raw: self.identity_raw.clone() })
    }

    async fn hist_ids(&self, security: &str, anchor: &str, _start: NaiveDate)
        -> AppResult<Vec<HistIdRow>>
    {
        if anchor.trim().is_empty() {
            return Err(AppError::Validation(
                "hist_ids requires an anchoring identifier (P0 6.4)".into()));
        }
        self.record(&format!("hist_ids:{security}"));
        Ok(parse_hist_ids(&self.hist_ids_raw))
    }

    async fn instrument_list(&self, query: &str, _yk: Option<&str>, _max: u32)
        -> AppResult<Answered<Vec<Candidate>>>
    {
        self.record(&format!("instrument_list:{query}"));
        Ok(Answered { parsed: parse_list(&self.list_raw), raw: self.list_raw.clone() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HISTIDS: &str = include_str!(
        "../../docs/superpowers/specs/blpapi-facts/histids_report.json");

    fn capture(key: &str) -> serde_json::Value {
        let all: serde_json::Value = serde_json::from_str(HISTIDS).unwrap();
        all[key].clone()
    }

    /// The P0 capture, replayed. If the parse breaks, this breaks -- no Terminal
    /// required and no fixture invented for the occasion.
    #[test]
    fn hist_id_rows_are_parsed_from_the_p0_capture() {
        let raw = capture("META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', \
                           'HISTORICAL_STARTING_IDENTIFIER']");
        let rows = parse_hist_ids(&raw);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].old_id, "FB");
        assert_eq!(rows[0].new_id, "META");
        assert_eq!(rows[0].date, "2022-06-09".parse::<chrono::NaiveDate>().unwrap());
        assert_eq!(rows[0].action_id.as_deref(), Some("228233742"));
    }

    /// P0 6.4, the trap this whole anchoring discipline exists for.
    #[test]
    fn the_unanchored_capture_describes_a_different_company() {
        let raw = capture("META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT']");
        let rows = parse_hist_ids(&raw);
        assert_eq!(rows[0].new_id, "METV",
                   "unanchored, Bloomberg answers about the Roundhill ETF");
    }

    #[test]
    fn an_identity_block_is_parsed_and_missing_fields_stay_none() {
        let raw = serde_json::json!([{"securityData": [{
            "security": "AAPL US Equity",
            "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {
                "ID_BB_GLOBAL": "BBG000B9XRY4",
                "ID_BB_UNIQUE": "EQ0010169500001000",
                "ID_ISIN": "US0378331005",
                "EXCH_CODE": "US",
                "CRNCY": "USD",
                "CNTRY_ISSUE_ISO": "US",
                "SECURITY_TYP2": "Common Stock",
                "MARKET_SECTOR_DES": "Equity",
                "NAME": "APPLE INC",
                "LISTING_DATE": "1980-12-12"
            }}]}]);
        let blocks = parse_identity(&raw);
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.security, "AAPL US Equity");
        assert_eq!(b.figi.as_deref(), Some("BBG000B9XRY4"));
        assert_eq!(b.bbg_unique.as_deref(), Some("EQ0010169500001000"),
                   "ID_BB_UNIQUE rides free in the same ReferenceDataRequest");
        assert_eq!(b.listing_date, Some("1980-12-12".parse().unwrap()));
        assert_eq!(b.inactive_date, None, "an absent field is None, never a default");
        assert_eq!(b.status, None);
    }

    #[test]
    fn a_security_error_yields_no_identity_block() {
        let raw = serde_json::json!([{"securityData": [{
            "security": "NOPE US Equity",
            "securityError": {"category": "BAD_SEC",
                              "subcategory": "INVALID_SECURITY",
                              "message": "Unknown/Invalid Security"},
            "fieldData": {}, "fieldExceptions": [], "sequenceNumber": 0}]}]);
        assert!(parse_identity(&raw).is_empty(),
                "a rejected security must not become a half-populated instrument");
    }

    /// The raw form is normalised here, in Rust, where the regression test for
    /// the doubled-yellow-key defect already lives.
    #[test]
    fn list_results_become_candidates_with_usable_security_strings() {
        let raw = serde_json::json!([{"results": [
            {"security": "AAPL US<equity>", "description": "Apple Inc"},
            {"security": "AAPL LN<equity>", "description": "Apple Inc"},
            {"security": "AAPL US 08/21/26 C400<equity>", "description": "Apple Inc call"},
            {"security": "GARBAGE<nonsense>", "description": "unaddressable"}
        ]}]);
        let cands = parse_list(&raw);
        let secs: Vec<&str> = cands.iter().map(|c| c.security.as_str()).collect();
        assert_eq!(secs, ["AAPL US Equity", "AAPL LN Equity",
                          "AAPL US 08/21/26 C400 Equity"],
                   "an unaddressable candidate is dropped; options survive here \
                    and are filtered at scoring time");
        assert_eq!(cands[0].exchange.as_deref(), Some("US"),
                   "the exchange code is read off the security string");
    }

    /// The hit-ledger accounting is a promise the budget screen makes to the
    /// user, so it is pinned rather than left to whatever the seam happens to
    /// compute. One hit per security-field pair for a reference request,
    /// matching `budget::estimate_eod_hits`; one for a bulk history request.
    #[test]
    fn the_wire_seam_charges_one_hit_per_security_field_pair() {
        assert_eq!(IDENTITY_FIELDS.len(), 12);
        assert_eq!(identity_hit_cost(1), 12, "one instrument resolved = 12 hits");
        assert_eq!(identity_hit_cost(3), 36);
        assert_eq!(identity_hit_cost(0), 0, "a request for nothing costs nothing");
        assert_eq!(HIST_IDS_HIT_COST, 1);
    }

    #[tokio::test]
    async fn the_mock_fetcher_replays_a_capture() {
        let mock = MockMasterFetcher::from_capture(HISTIDS);
        let rows = mock.hist_ids("META US Equity", "META US Equity",
                                 "2000-01-01".parse().unwrap()).await.unwrap();
        assert_eq!(rows[0].new_id, "META");
    }

    /// "A refused request must not cost a Bloomberg hit." A blank anchor is
    /// rejected before the mock records anything -- call_count staying 0 is
    /// the whole assertion.
    #[tokio::test]
    async fn a_blank_anchor_is_refused_before_any_call_is_recorded() {
        let mock = MockMasterFetcher::from_capture(HISTIDS);
        let result = mock.hist_ids("META US Equity", "   ",
                                   "2000-01-01".parse().unwrap()).await;
        assert!(result.is_err());
        assert_eq!(mock.call_count(), 0,
                   "a rejected request must not reach the mock's call log, \
                    which stands in for the Bloomberg wire");
    }

    /// Task 7's local-resolution test asserts Bloomberg was NOT called by
    /// reading this counter; this pins that it counts correctly in the first
    /// place.
    #[tokio::test]
    async fn call_count_tracks_every_recorded_call() {
        let mock = MockMasterFetcher::from_capture(HISTIDS);
        mock.identity(&["AAPL US Equity".to_string()]).await.unwrap();
        mock.instrument_list("AAPL", None, 20).await.unwrap();
        assert_eq!(mock.call_count(), 2);
    }
}
