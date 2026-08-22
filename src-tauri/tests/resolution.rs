mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::error::{AppError, AppResult};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::master_fetch::{
    Answered, HistIdRow, IdentityBlock, MasterFetcher, MockMasterFetcher};
use getbloomdata_lib::resolution::engine::{self, Resolution, ResolveInput};
use getbloomdata_lib::resolution::score::{Candidate, Hints};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

fn input(raw: &str) -> ResolveInput {
    ResolveInput {
        raw: raw.into(),
        yellow_key: "Equity".into(),
        hints: Hints::default(),
        as_of: d("2026-08-19"),
        decided_by: "auto".into(),
    }
}

/// `US0378331005` used to be hardcoded here and shared by every caller,
/// which meant three different instruments in `bloom_test` all wore the same
/// ISIN -- invisible until some later test probed `isin` and hit a false
/// local ambiguity. Each call gets its own via `uniq()`.
fn identity_mock(security: &str, figi: &str, exch: &str) -> MockMasterFetcher {
    identity_mock_dated(security, figi, exch, Some("1980-12-12"), None)
}

/// Same shape as `identity_mock`, with LISTING_DATE / INACTIVE_DATE under the
/// caller's control -- for exercising `bind_identity`'s validity-period
/// derivation, which no other test drives Bloomberg fields for.
fn identity_mock_dated(security: &str, figi: &str, exch: &str,
                       listing_date: Option<&str>, inactive_date: Option<&str>)
    -> MockMasterFetcher
{
    let mut field_data = serde_json::json!({
        "ID_BB_GLOBAL": figi, "ID_ISIN": uniq("US0378331005"),
        "EXCH_CODE": exch, "CRNCY": "USD", "CNTRY_ISSUE_ISO": "US",
        "SECURITY_TYP2": "Common Stock", "MARKET_SECTOR_DES": "Equity",
        "NAME": "APPLE INC",
    });
    if let Some(v) = listing_date {
        field_data["LISTING_DATE"] = serde_json::json!(v);
    }
    if let Some(v) = inactive_date {
        field_data["INACTIVE_DATE"] = serde_json::json!(v);
    }
    MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": security, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": field_data}]}]),
        ..Default::default()
    }
}

/// Step 2 of the pipeline. The hit budget depends on this being true: an
/// instrument already in the master is never asked about again.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_known_alias_resolves_locally_and_calls_bloomberg_zero_times() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let ticker = uniq("AAPL");
    let security = format!("{ticker} US Equity");
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: security.clone(),
        exch_code: Some("US".into()), valid_from: d("1980-12-12"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();

    let mock = MockMasterFetcher::default();
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();

    match r {
        Resolution::Bound { instrument_id, method, decision_id } => {
            assert_eq!(instrument_id, inst.instrument_id);
            assert_eq!(method, "local_alias");
            // Even the free path is recorded, so the audit trail has no holes.
            let m: String = sqlx::query_scalar(
                "SELECT method FROM resolution_decision WHERE id = $1")
                .bind(decision_id).fetch_one(&pool).await.unwrap();
            assert_eq!(m, "local_alias");
        }
        other => panic!("expected Bound, got {other:?}"),
    }
    assert_eq!(mock.call_count(), 0, "a known instrument costs nothing");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_unknown_identifier_resolves_through_a_reference_request() {
    let pool = common::pool().await;
    let ticker = uniq("ZZTOP");
    let figi = uniq("BBG000TESTAA");
    let mock = identity_mock(&format!("{ticker} US Equity"), &figi, "US");
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();
    let Resolution::Bound { instrument_id, method, .. } = r else {
        panic!("expected Bound, got {r:?}")
    };
    assert_eq!(method, "bloomberg_ref");

    // The identity block became aliases and attributes, not columns.
    let aliases = store::aliases(&pool, instrument_id).await.unwrap();
    let types: Vec<&str> = aliases.iter().map(|a| a.id_type.as_str()).collect();
    assert!(types.contains(&"bdp_security"));
    assert!(types.contains(&"figi"));
    assert!(types.contains(&"isin"));
    let attrs = store::attrs(&pool, instrument_id, d("2026-08-19")).await.unwrap();
    assert!(attrs.iter().any(|a| a.attr == "name" && a.value == "APPLE INC"));
    assert!(attrs.iter().any(|a| a.attr == "currency" && a.value == "USD"));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_unedited_bloomberg_response_is_stored_with_the_decision() {
    let pool = common::pool().await;
    let ticker = uniq("ZZTOP2");
    let figi = uniq("BBG000TESTAB");
    let mock = identity_mock(&format!("{ticker} US Equity"), &figi, "US");
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();
    let Resolution::Bound { decision_id, .. } = r else { panic!("expected Bound") };
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT bbg_response FROM resolution_decision WHERE id = $1")
        .bind(decision_id).fetch_one(&pool).await.unwrap();
    assert_eq!(raw[0]["securityData"][0]["fieldData"]["NAME"], "APPLE INC",
               "what Bloomberg said is recoverable, not just what we concluded");
}

/// Step 6. Two survivors bind nothing -- the whole point of the phase.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_ambiguous_result_opens_a_review_and_binds_nothing() {
    let pool = common::pool().await;
    let ticker = uniq("AAPL");
    let mock = MockMasterFetcher {
        // No identity block: the reference request found nothing usable...
        identity_raw: serde_json::json!([]),
        // ...so step 4 searches, and two listings come back.
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Apple Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Apple Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, candidates, .. } = r else {
        panic!("expected NeedsReview, got {r:?}")
    };
    assert_eq!(candidates.len(), 2);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "pending");

    // Scoped to the securities this test itself produced -- a global count
    // would be false the moment any other test has run.
    let bound_us = store::find_by_alias(&pool, "bdp_security",
        &format!("{ticker} US Equity"), d("2026-08-19")).await.unwrap();
    let bound_ln = store::find_by_alias(&pool, "bdp_security",
        &format!("{ticker} LN Equity"), d("2026-08-19")).await.unwrap();
    assert!(bound_us.is_none() && bound_ln.is_none(),
            "nothing binds while a human has not chosen");
}

/// Two live instruments can legitimately wear the same identifier -- BMW in
/// Frankfurt and in the US. That ambiguity is entirely local: a Bloomberg
/// call cannot resolve it, so none is made, and nothing binds silently.
#[tokio::test]
#[ignore = "requires postgres"]
async fn two_instruments_sharing_a_local_alias_open_a_review_without_calling_bloomberg() {
    let pool = common::pool().await;
    let ticker = uniq("BMW");
    let security = format!("{ticker} GY Equity");

    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    for inst in [&a, &b] {
        store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
            id_type: "bdp_security".into(), value: security.clone(),
            exch_code: Some("GY".into()), valid_from: d("2000-01-01"), valid_to: None,
            source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mock = MockMasterFetcher::default();
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} GY"))).await.unwrap();
    let Resolution::NeedsReview { review_id, candidates, .. } = r else {
        panic!("expected NeedsReview, got {r:?}")
    };
    assert_eq!(candidates.len(), 2, "both existing instruments surface as candidates");
    assert_eq!(mock.call_count(), 0,
               "a local ambiguity cannot be resolved by a Bloomberg call, so none is made");

    let method: String = sqlx::query_scalar(
        "SELECT method FROM resolution_decision d
           JOIN resolution_review r ON r.decision_id = d.id WHERE r.id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(method, "local_alias");
    let chosen: Option<i64> = sqlx::query_scalar(
        "SELECT d.chosen_instrument_id FROM resolution_decision d
           JOIN resolution_review r ON r.decision_id = d.id WHERE r.id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(chosen, None, "nothing is bound while ambiguous");
}

/// A small fetcher keyed on the requested security string, standing in for
/// the two different requests step 5 makes: the step-3 probe on the plain
/// ticker ("<TICKER> Equity", which must come back empty so the pipeline
/// proceeds to a search) and the scored winner after a hint disqualifies one
/// candidate ("<TICKER> LN Equity", which must come back populated).
/// `MockMasterFetcher` cannot do this -- it replays the same canned
/// `identity_raw` for every call, so it cannot tell those two requests apart.
struct KeyedFetcher {
    ticker: String,
    figi: String,
    isin: String,
    /// Every call recorded, so the test can pin exactly how many Bloomberg
    /// requests the Unique path costs -- mirroring MockMasterFetcher's own
    /// call_count(), which this fetcher cannot reuse because it needs to
    /// answer differently per requested security.
    calls: std::sync::Mutex<u32>,
}

impl KeyedFetcher {
    fn new(ticker: String, figi: String, isin: String) -> Self {
        Self { ticker, figi, isin, calls: std::sync::Mutex::new(0) }
    }

    fn call_count(&self) -> u32 {
        *self.calls.lock().unwrap()
    }

    fn record(&self) {
        *self.calls.lock().unwrap() += 1;
    }
}

impl MasterFetcher for KeyedFetcher {
    async fn identity(&self, securities: &[String]) -> AppResult<Answered<Vec<IdentityBlock>>> {
        self.record();
        let sec = securities.first().cloned().unwrap_or_default();
        if sec == format!("{} LN Equity", self.ticker) {
            // A realistic wire shape, not a re-serialized IdentityBlock --
            // the same nested securityData/fieldData proof the bloomberg_ref
            // path already has, now covering the bloomberg_list path too.
            let raw = serde_json::json!([{"securityData": [{
                "security": sec, "fieldExceptions": [], "sequenceNumber": 0,
                "fieldData": {
                    "ID_BB_GLOBAL": self.figi, "ID_ISIN": self.isin,
                    "EXCH_CODE": "LN", "CRNCY": "GBP", "CNTRY_ISSUE_ISO": "GB",
                    "SECURITY_TYP2": "Common Stock", "MARKET_SECTOR_DES": "Equity",
                    "NAME": "TEST INC", "LISTING_DATE": "1990-01-01"}}]}]);
            let parsed = getbloomdata_lib::master_fetch::parse_identity(&raw);
            Ok(Answered { parsed, raw })
        } else {
            // The step-3 probe on the bare ticker: nothing usable, so the
            // pipeline must fall through to a search.
            Ok(Answered { parsed: vec![], raw: serde_json::json!([]) })
        }
    }

    async fn hist_ids(&self, _security: &str, anchor: &str, _start: NaiveDate)
        -> AppResult<Vec<HistIdRow>>
    {
        self.record();
        if anchor.trim().is_empty() {
            return Err(AppError::Validation("anchor required".into()));
        }
        Ok(vec![])
    }

    async fn corp_actions(&self, _securities: &[String])
        -> AppResult<Answered<getbloomdata_lib::master_fetch::CorpActionsTables>>
    {
        self.record();
        Ok(Answered { parsed: Default::default(), raw: serde_json::json!([]) })
    }

    async fn market_status(&self, _securities: &[String])
        -> AppResult<Answered<Vec<(String, String)>>>
    {
        self.record();
        Ok(Answered { parsed: vec![], raw: serde_json::json!([]) })
    }

    async fn ma_deals(&self, _security: &str)
        -> AppResult<Answered<getbloomdata_lib::master_fetch::MaDealsOutcome>>
    {
        self.record();
        Ok(Answered { parsed: Default::default(), raw: serde_json::json!([]) })
    }

    async fn action_terms(&self, _action_id: &str)
        -> AppResult<Answered<Option<getbloomdata_lib::master_fetch::ActionTerms>>>
    {
        self.record();
        Ok(Answered { parsed: None, raw: serde_json::json!([]) })
    }

    async fn identity_sweep(&self, _securities: &[String], _sweep: &str)
        -> AppResult<Answered<Vec<getbloomdata_lib::master_fetch::SweepAnswer>>>
    {
        self.record();
        Ok(Answered { parsed: Vec::new(), raw: serde_json::json!([]) })
    }

    async fn instrument_list(&self, _query: &str, _yellow_key_filter: Option<&str>,
                             _max_results: u32) -> AppResult<Answered<Vec<Candidate>>>
    {
        self.record();
        let parsed = vec![
            Candidate {
                security: format!("{} US Equity", self.ticker),
                description: "Test Inc".into(),
                exchange: Some("US".into()), country: None, currency: None,
                asset_class: None, figi: None,
            },
            Candidate {
                security: format!("{} LN Equity", self.ticker),
                description: "Test Inc".into(),
                exchange: Some("LN".into()), country: None, currency: None,
                asset_class: None, figi: None,
            },
        ];
        let raw = serde_json::to_value(&parsed).unwrap_or_default();
        Ok(Answered { parsed, raw })
    }
}

/// Step 5. One survivor after scoring binds without a human.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_hint_that_leaves_one_survivor_binds_without_review() {
    let pool = common::pool().await;
    let ticker = uniq("ZBBB");
    let fetcher = KeyedFetcher::new(ticker.clone(), uniq("BBG000TESTZB"),
                                    uniq("GB0002634946"));

    let mut inp = input(&ticker);
    inp.hints.exchange = Some("LN".into());
    let r = engine::resolve(&pool, &fetcher, &inp).await.unwrap();

    match r {
        Resolution::Bound { method, .. } => assert_eq!(method, "bloomberg_list",
            "the exchange hint narrowed the search to one survivor, which the \
             engine then re-resolved for real"),
        other => panic!("expected Bound with method bloomberg_list, got {other:?}"),
    }
    // Pins the exact Bloomberg cost of this path: the mandatory step-3
    // reference probe on the bare ticker (comes back empty, no exchange
    // qualifier), the step-4 search, and the step-5 confirming reference call
    // for the scored winner. Nothing here should be able to grow this
    // silently -- e.g. a second identity() call snuck into scoring.
    //
    // Three, not four: the anchored identifier-history request that used to
    // fire after every successful bind is gone (P0 §6.5 -- resolution knows
    // the chain's END and the field is anchored on its START, so it returned
    // another company's chain). It is now an explicit user action.
    assert_eq!(fetcher.call_count(), 3,
               "1 failed reference probe + 1 search + 1 confirming reference call, \
                and NO identifier-history request");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn resolving_a_review_binds_the_chosen_candidate_and_closes_it() {
    let pool = common::pool().await;
    let ticker = uniq("ZCCC");
    let list_mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &list_mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };

    // Selecting a suggestion runs the full resolution again -- it does not
    // bind the clicked string directly, so the review is resolved against a
    // fetcher that can actually answer for the chosen security.
    let chosen = format!("{ticker} US Equity");
    let figi = uniq("BBG000TESTZC");
    let resolve_mock = identity_mock(&chosen, &figi, "US");
    let iid = engine::resolve_review(&pool, &resolve_mock, review_id, &chosen, "laurent",
                                     d("2026-08-19"))
        .await.unwrap();

    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "resolved");
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &chosen,
                                    d("2026-08-19")).await.unwrap(), Some(iid));

    // The chosen security was resolved for real: full attrs and aliases, not
    // a bare security string.
    let attrs = store::attrs(&pool, iid, d("2026-08-19")).await.unwrap();
    assert!(attrs.iter().any(|a| a.attr == "name" && a.value == "APPLE INC"));
    let aliases = store::aliases(&pool, iid).await.unwrap();
    assert!(aliases.iter().any(|a| a.id_type == "figi" && a.value == figi));

    assert!(engine::pending_reviews(&pool).await.unwrap()
                .iter().all(|p| p.review_id != review_id));
}

/// If Bloomberg has nothing to say about the chosen security, the human
/// decision is still recorded -- refusing would discard a real decision over
/// a transient Bloomberg gap -- but the fallback is visible in the audit
/// trail rather than silently indistinguishable from a real resolution.
#[tokio::test]
#[ignore = "requires postgres"]
async fn resolving_a_review_falls_back_to_a_bare_block_when_bloomberg_has_nothing() {
    let pool = common::pool().await;
    let ticker = uniq("ZDDD");
    let list_mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &list_mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };

    let chosen = format!("{ticker} US Equity");
    // Same mock: identity_raw is still empty, so resolve_review's own
    // identity() call also comes back empty.
    let iid = engine::resolve_review(&pool, &list_mock, review_id, &chosen, "laurent",
                                     d("2026-08-19"))
        .await.unwrap();

    assert_eq!(store::find_by_alias(&pool, "bdp_security", &chosen,
                                    d("2026-08-19")).await.unwrap(), Some(iid));
    let candidates: serde_json::Value = sqlx::query_scalar(
        "SELECT candidates FROM resolution_decision
          WHERE method = 'manual' AND chosen_instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(candidates["bloomberg_fallback"], true,
               "the fallback to a bare block must stay visible in the audit trail");
}

/// Critical 2: INACTIVE_DATE with no LISTING_DATE at all is routine outside
/// cash equities. `valid_from = today, valid_to = a past inactive date`
/// would violate `CHECK (valid_from < valid_to)` and die on SQLSTATE 23514;
/// the honest floor instead is the day before the instrument died.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_inactive_date_with_no_listing_date_still_gets_a_non_empty_period() {
    let pool = common::pool().await;
    let ticker = uniq("ZIII");
    let mock = identity_mock_dated(&format!("{ticker} US Equity"), &uniq("BBG000TESTZI"),
                                   "US", None, Some("2010-06-30"));
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();
    let Resolution::Bound { instrument_id, .. } = r else { panic!("expected Bound, got {r:?}") };

    let aliases = store::aliases(&pool, instrument_id).await.unwrap();
    let sec = aliases.iter().find(|a| a.id_type == "bdp_security").unwrap();
    assert_eq!(sec.valid_to, d("2010-06-30"));
    assert_eq!(sec.valid_from, d("2010-06-29"),
               "no LISTING_DATE, so the day before INACTIVE_DATE is the honest floor");
}

/// Critical 2: LISTING_DATE == INACTIVE_DATE produces an empty period under
/// the naive derivation and must not be allowed to reach the insert.
#[tokio::test]
#[ignore = "requires postgres"]
async fn equal_listing_and_inactive_dates_still_get_a_non_empty_period() {
    let pool = common::pool().await;
    let ticker = uniq("ZJJJ");
    let mock = identity_mock_dated(&format!("{ticker} US Equity"), &uniq("BBG000TESTZJ"),
                                   "US", Some("2010-06-30"), Some("2010-06-30"));
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();
    let Resolution::Bound { instrument_id, .. } = r else { panic!("expected Bound, got {r:?}") };

    let aliases = store::aliases(&pool, instrument_id).await.unwrap();
    let sec = aliases.iter().find(|a| a.id_type == "bdp_security").unwrap();
    assert_eq!(sec.valid_to, d("2010-06-30"));
    assert_eq!(sec.valid_from, d("2010-06-29"));
}

/// Critical 2 / Important 5: the normal case, LISTING_DATE strictly before
/// INACTIVE_DATE -- and the attribute-closing half of Important 5. No prior
/// test ever supplied INACTIVE_DATE at all, so `set_attr`'s always-open
/// `valid_to` (forever, absent a later period) was never checked against it.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_delisted_instrument_s_aliases_and_attributes_both_stop_at_its_inactive_date() {
    let pool = common::pool().await;
    let ticker = uniq("ZKKK");
    let mock = identity_mock_dated(&format!("{ticker} US Equity"), &uniq("BBG000TESTZK"),
                                   "US", Some("1995-01-01"), Some("2010-06-30"));
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();
    let Resolution::Bound { instrument_id, .. } = r else { panic!("expected Bound, got {r:?}") };

    let aliases = store::aliases(&pool, instrument_id).await.unwrap();
    let sec = aliases.iter().find(|a| a.id_type == "bdp_security").unwrap();
    assert_eq!(sec.valid_from, d("1995-01-01"), "the real listing date is used when it fits");
    assert_eq!(sec.valid_to, d("2010-06-30"));

    // While alive, the name is current.
    let live = store::attrs(&pool, instrument_id, d("2005-01-01")).await.unwrap();
    assert!(live.iter().any(|a| a.attr == "name" && a.value == "APPLE INC"));

    // After INACTIVE_DATE, the same attribute must not still read as current --
    // set_attr alone always leaves it open-ended; only close_attrs_at caps it.
    let after = store::attrs(&pool, instrument_id, d("2015-01-01")).await.unwrap();
    assert!(!after.iter().any(|a| a.attr == "name"),
            "a delisted instrument's attributes must stop being current at its inactive date");
}

/// Important 3, second half: resolving an already-resolved review must not
/// be allowed to mint a second instrument for the same identifier.
#[tokio::test]
#[ignore = "requires postgres"]
async fn resolve_review_refuses_a_review_that_is_already_resolved() {
    let pool = common::pool().await;
    let ticker = uniq("ZLLL");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };

    let chosen = format!("{ticker} US Equity");
    engine::resolve_review(&pool, &mock, review_id, &chosen, "laurent", d("2026-08-19")).await.unwrap();

    let err = engine::resolve_review(&pool, &mock, review_id, &chosen, "laurent", d("2026-08-19")).await
        .unwrap_err();
    assert!(matches!(err, AppError::Validation(_)),
            "a second resolution of the same review must be refused, not silently repeated");
}

/// Important 3, second half: a review the user rejected is not pending
/// either -- resolving it anyway would resurrect a decision a human closed.
#[tokio::test]
#[ignore = "requires postgres"]
async fn resolve_review_refuses_a_rejected_review() {
    let pool = common::pool().await;
    let ticker = uniq("ZMMM");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };
    engine::reject_review(&pool, review_id, "not a real security").await.unwrap();

    let chosen = format!("{ticker} US Equity");
    let err = engine::resolve_review(&pool, &mock, review_id, &chosen, "laurent", d("2026-08-19")).await
        .unwrap_err();
    assert!(matches!(err, AppError::Validation(_)));
}

/// Important 3, first half: bind_identity's no-FIGI dedup. Two independent
/// searches for the same identifier, each opening its own review, resolved
/// to the same security while Bloomberg stays silent both times (the
/// fallback-to-a-bare-block path) must bind ONE instrument, not two --
/// otherwise that identifier becomes a permanent local ambiguity the moment
/// the second review resolves.
#[tokio::test]
#[ignore = "requires postgres"]
async fn two_reviews_resolved_to_the_same_silent_security_bind_one_instrument() {
    let pool = common::pool().await;
    let ticker = uniq("ZNNN");
    let chosen = format!("{ticker} US Equity");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };

    let r1 = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id: review1, .. } = r1 else { panic!("expected review") };
    let iid1 = engine::resolve_review(&pool, &mock, review1, &chosen, "laurent", d("2026-08-19")).await.unwrap();

    // A second, independent search for the same raw identifier -- the bound
    // alias from the first review does not match this input's own probes
    // (it was built from the bare ticker, not "<ticker> US Equity"), so this
    // also reaches step 4 and opens a second review.
    let r2 = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id: review2, .. } = r2 else { panic!("expected review") };
    assert_ne!(review1, review2);
    let iid2 = engine::resolve_review(&pool, &mock, review2, &chosen, "laurent", d("2026-08-19")).await.unwrap();

    assert_eq!(iid1, iid2,
               "the same identifier must not mint two instruments even though Bloomberg \
                answered nothing both times");
}

/// Important 4: a bound instrument's manual decision must still show what
/// the human was choosing between and where that choice came from.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_manual_decision_records_the_review_and_the_original_candidates() {
    let pool = common::pool().await;
    let ticker = uniq("ZOOO");
    let list_mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &list_mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, decision_id, .. } = r else {
        panic!("expected review")
    };

    let chosen = format!("{ticker} US Equity");
    let resolve_mock = identity_mock(&chosen, &uniq("BBG000TESTZO"), "US");
    let iid = engine::resolve_review(&pool, &resolve_mock, review_id, &chosen, "laurent", d("2026-08-19"))
        .await.unwrap();

    let candidates: serde_json::Value = sqlx::query_scalar(
        "SELECT candidates FROM resolution_decision
          WHERE method = 'manual' AND chosen_instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(candidates["review_id"], review_id);
    assert_eq!(candidates["source_decision_id"], decision_id);
    assert_eq!(candidates["original_candidates"].as_array().unwrap().len(), 2,
               "what the human chose from -- including the candidate they rejected -- \
                must still be visible from the bound instrument's decision");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn nothing_found_is_recorded_as_a_decision_too() {
    let pool = common::pool().await;
    let ticker = uniq("QQQQZZZ");
    let mock = MockMasterFetcher::default();  // empty everything
    let r = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NotFound { decision_id } = r else { panic!("expected NotFound, got {r:?}") };
    let chosen: Option<i64> = sqlx::query_scalar(
        "SELECT chosen_instrument_id FROM resolution_decision WHERE id = $1")
        .bind(decision_id).fetch_one(&pool).await.unwrap();
    assert_eq!(chosen, None);
}

// ---------------------------------------------------------------------------
// C1: a rename must be able to land on an instrument that already exists.
// ---------------------------------------------------------------------------

/// Same FIGI, different security string. `bind_identity` used to return the
/// existing instrument id before writing anything at all, so an instrument
/// bound while it wore `FB US Equity` went on answering `FB US Equity`
/// forever: `current_security` handed a dead ticker to every fetch and the
/// series stopped without one error. This is the branch's headline promise.
#[tokio::test]
#[ignore = "requires postgres"]
async fn re_resolving_the_same_figi_under_a_new_security_records_the_rename() {
    let pool = common::pool().await;
    let old_ticker = uniq("FBZ");
    let new_ticker = uniq("METAZ");
    let figi = uniq("BBG000RENAME");
    let old_security = format!("{old_ticker} US Equity");
    let new_security = format!("{new_ticker} US Equity");

    let first = identity_mock(&old_security, &figi, "US");
    let r = engine::resolve(&pool, &first, &input(&format!("{old_ticker} US")))
        .await.unwrap();
    let Resolution::Bound { instrument_id, .. } = r else { panic!("expected Bound, got {r:?}") };
    assert_eq!(store::current_security(&pool, instrument_id, d("2026-08-19"))
                   .await.unwrap().as_deref(), Some(old_security.as_str()));

    // Bloomberg now answers for a different security string with the SAME
    // FIGI -- which is exactly what a rename looks like from here.
    let mut second = identity_mock(&new_security, &figi, "US");
    second.identity_raw[0]["securityData"][0]["fieldData"]["NAME"] =
        serde_json::json!("META PLATFORMS INC");
    let r2 = engine::resolve(&pool, &second, &input(&format!("{new_ticker} US")))
        .await.unwrap();
    let Resolution::Bound { instrument_id: same, .. } = r2 else {
        panic!("expected Bound, got {r2:?}")
    };
    assert_eq!(same, instrument_id, "one FIGI is one instrument, not two");

    // Two bdp_security periods, the earlier one closed where the later starts.
    let aliases = store::aliases(&pool, instrument_id).await.unwrap();
    let mut secs: Vec<_> = aliases.iter()
        .filter(|a| a.id_type == "bdp_security")
        .collect();
    secs.sort_by_key(|a| a.valid_from);
    assert_eq!(secs.len(), 2, "a rename is two periods, never an edit: {secs:?}");
    assert_eq!(secs[0].value, old_security);
    assert_eq!(secs[1].value, new_security);
    assert_eq!(secs[0].valid_to, secs[1].valid_from,
               "the old period ends exactly where the new one begins");
    assert_eq!(secs[1].valid_from, d("2026-08-19"), "closed at today, not backdated");
    assert_eq!(secs[1].valid_to, store::forever());

    // The current security is the NEW one -- the whole point.
    assert_eq!(store::current_security(&pool, instrument_id, d("2026-08-19"))
                   .await.unwrap().as_deref(), Some(new_security.as_str()));
    // ...and the old string still resolves to the same instrument in its own era.
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &old_security,
                                    d("2020-01-01")).await.unwrap(), Some(instrument_id));

    // Attributes were refreshed too, not left at the values of the first bind.
    let attrs = store::attrs(&pool, instrument_id, d("2026-08-19")).await.unwrap();
    assert!(attrs.iter().any(|a| a.attr == "name" && a.value == "META PLATFORMS INC"),
            "the identity block's attributes are re-run on reconciliation: {attrs:?}");
}

/// The reconciliation must be a no-op when nothing actually changed --
/// otherwise every re-resolution would pile up a fresh alias period and a
/// superseded attribute row for the same facts. (Step 2 answers first here,
/// which is the strongest form of that guarantee: no call is made at all.)
#[tokio::test]
#[ignore = "requires postgres"]
async fn re_resolving_an_unchanged_identity_writes_no_new_period() {
    let pool = common::pool().await;
    let ticker = uniq("SAMEZ");
    let figi = uniq("BBG000SAMEZ");
    let security = format!("{ticker} US Equity");
    let mock = identity_mock(&security, &figi, "US");

    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} US"))).await.unwrap();
    let Resolution::Bound { instrument_id, .. } = r else { panic!("expected Bound") };
    let before = store::aliases(&pool, instrument_id).await.unwrap().len();

    let r2 = engine::resolve(&pool, &mock, &input(&format!("{ticker} US Equity")))
        .await.unwrap();
    let Resolution::Bound { instrument_id: same, method, .. } = r2 else {
        panic!("expected Bound")
    };
    assert_eq!(same, instrument_id);
    assert_eq!(method, "local_alias", "already local: no Bloomberg call at all");
    assert_eq!(store::aliases(&pool, instrument_id).await.unwrap().len(), before,
               "nothing changed, so nothing was written");
}

// ---------------------------------------------------------------------------
// I1: a locally ambiguous review is a local re-point, not a Bloomberg call.
// ---------------------------------------------------------------------------

/// The placeholder `local_ambiguity_candidates` writes when an existing
/// instrument has no current security string must be unbindable, even if the
/// UI regresses and hands it back. Before this guard, clicking "This one"
/// spent a real Bloomberg call on `instrument #42`, got nothing, took the
/// bare-block fallback, and minted a permanent instrument whose bdp_security
/// alias was that literal text.
#[tokio::test]
#[ignore = "requires postgres"]
async fn resolve_review_refuses_a_chosen_security_that_is_not_a_security_string() {
    let pool = common::pool().await;
    let ticker = uniq("ZPPP");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };
    let calls_before = mock.call_count();

    for bad in ["instrument #42", "AAPL US", "", "   ", "AAPL US Nonsense"] {
        let err = engine::resolve_review(&pool, &mock, review_id, bad, "laurent", d("2026-08-19"))
            .await.unwrap_err();
        assert!(matches!(err, AppError::Validation(_)),
                "{bad:?} must be refused as a security string, got {err:?}");
    }
    assert_eq!(mock.call_count(), calls_before,
               "a refused chosen_security must not cost a Bloomberg hit");

    // And the review is untouched -- still available for a real decision.
    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "pending");
}

/// Spec 7: "a locally ambiguous identifier -- none; a Bloomberg call cannot
/// resolve a local ambiguity." Until `resolve_review_local` existed, the only
/// action the review screen could take on such a row was `resolve_review`,
/// which always calls out -- so the free path was documented and unreachable.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_local_ambiguity_is_re_pointed_at_an_existing_instrument_for_free() {
    let pool = common::pool().await;
    let ticker = uniq("BMWZ");
    let security = format!("{ticker} GY Equity");

    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    for inst in [&a, &b] {
        store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
            id_type: "bdp_security".into(), value: security.clone(),
            exch_code: Some("GY".into()), valid_from: d("2000-01-01"), valid_to: None,
            source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mock = MockMasterFetcher::default();
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} GY"))).await.unwrap();
    let Resolution::NeedsReview { review_id, candidates, .. } = r else {
        panic!("expected NeedsReview, got {r:?}")
    };
    // Each scored entry names the instrument it stands for, so the screen can
    // hand back an id rather than a string.
    let ids: Vec<i64> = candidates.iter().filter_map(|c| c.instrument_id).collect();
    assert_eq!(ids.len(), 2, "every local-ambiguity candidate carries its instrument id");
    assert!(ids.contains(&a.instrument_id) && ids.contains(&b.instrument_id));

    // The stored decision carries the marker the UI branches on.
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT d.candidates FROM resolution_decision d
           JOIN resolution_review r ON r.decision_id = d.id WHERE r.id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(stored["local_ambiguity"], true,
               "without this marker the review screen renders it as a Bloomberg \
                candidate list and offers a button that spends a call");

    let chosen = engine::resolve_review_local(&pool, review_id, b.instrument_id, "laurent")
        .await.unwrap();
    assert_eq!(chosen, b.instrument_id);
    assert_eq!(mock.call_count(), 0, "a local re-point costs zero Bloomberg calls");

    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "resolved");
    let decision: (Option<i64>, serde_json::Value) = sqlx::query_as(
        "SELECT chosen_instrument_id, candidates FROM resolution_decision
          WHERE method = 'manual' AND chosen_instrument_id = $1
          ORDER BY id DESC LIMIT 1")
        .bind(b.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(decision.0, Some(b.instrument_id));
    assert_eq!(decision.1["local_repoint"], true);
    assert_eq!(decision.1["bloomberg_calls"], 0);
}

/// The re-point may only name an instrument the decision actually offered.
/// Otherwise a caller could point an input at something the user never saw,
/// and the closed review would look identical afterwards.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_local_re_point_refuses_an_instrument_that_was_never_a_candidate() {
    let pool = common::pool().await;
    let ticker = uniq("BMWY");
    let security = format!("{ticker} GY Equity");
    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let outsider = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    for inst in [&a, &b] {
        store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
            id_type: "bdp_security".into(), value: security.clone(),
            exch_code: Some("GY".into()), valid_from: d("2000-01-01"), valid_to: None,
            source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
    }
    tx.commit().await.unwrap();

    let mock = MockMasterFetcher::default();
    let r = engine::resolve(&pool, &mock, &input(&format!("{ticker} GY"))).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };

    let err = engine::resolve_review_local(&pool, review_id, outsider.instrument_id, "laurent")
        .await.unwrap_err();
    assert!(matches!(err, AppError::Validation(_)));
    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "pending", "a refused re-point must not close the review");
}

// ---------------------------------------------------------------------------
// I3: reject_review has to guard on status like resolve_review does.
// ---------------------------------------------------------------------------

/// Rejecting an already-resolved review used to succeed silently, flipping
/// `status` and OVERWRITING `note` -- destroying the record of what was bound
/// and by whom, while the instrument stayed in the book.
#[tokio::test]
#[ignore = "requires postgres"]
async fn reject_review_refuses_a_review_that_is_already_resolved() {
    let pool = common::pool().await;
    let ticker = uniq("ZRRR");
    let list_mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &list_mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };

    let chosen = format!("{ticker} US Equity");
    let resolve_mock = identity_mock(&chosen, &uniq("BBG000TESTZR"), "US");
    engine::resolve_review(&pool, &resolve_mock, review_id, &chosen, "laurent", d("2026-08-19"))
        .await.unwrap();
    let note_before: String = sqlx::query_scalar(
        "SELECT note FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();

    let err = engine::reject_review(&pool, review_id, "changed my mind").await.unwrap_err();
    assert!(matches!(err, AppError::Validation(_)),
            "rejecting a resolved review must error, not silently succeed");

    let (status, note): (String, String) = sqlx::query_as(
        "SELECT status, note FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "resolved");
    assert_eq!(note, note_before, "the record of the binding survives intact");
}

/// The same guard, from the other direction: rejecting twice.
#[tokio::test]
#[ignore = "requires postgres"]
async fn reject_review_refuses_a_review_that_is_already_rejected() {
    let pool = common::pool().await;
    let ticker = uniq("ZSSS");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test Inc"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };
    engine::reject_review(&pool, review_id, "first rejection").await.unwrap();
    let err = engine::reject_review(&pool, review_id, "second rejection").await.unwrap_err();
    assert!(matches!(err, AppError::Validation(_)));
    let note: String = sqlx::query_scalar("SELECT note FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(note, "first rejection", "the original note is not overwritten");
}

// ---------------------------------------------------------------------------
// auto_reresolve_invalid (post-run dead-security recovery)
// ---------------------------------------------------------------------------

/// Instrument wearing `figi` (write-once id + alias) and a live but
/// soon-dead bdp_security; a run row + an invalid_security issue against it.
async fn scaffold_dead_run(pool: &sqlx::PgPool, figi: Option<&str>, old_sec: &str)
    -> (i64, i64) {
    let inst = store::create(pool).await.unwrap();
    let iid = inst.instrument_id;
    if let Some(f) = figi {
        store::set_bloomberg_ids(pool, iid, Some(f), None).await.unwrap();
    }
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, iid, &NewAlias {
        id_type: "bdp_security".into(), value: old_sec.into(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    if let Some(f) = figi {
        store::insert_alias(&mut tx, iid, &NewAlias {
            id_type: "figi".into(), value: f.into(),
            exch_code: None, valid_from: d("2000-01-03"), valid_to: None,
            source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
    }
    tx.commit().await.unwrap();
    let run_id = insert_invalid_security_run(pool, iid).await;
    (iid, run_id)
}

async fn insert_invalid_security_run(pool: &sqlx::PgPool, iid: i64) -> i64 {
    let vname = uniq("autoreres");
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(&vname).fetch_one(pool).await.unwrap();
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','scheduled','partial') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
         VALUES ($1,$2,'warn','invalid_security','Unknown/Invalid Security')")
        .bind(run_id).bind(iid).execute(pool).await.unwrap();
    run_id
}

/// A run that saw invalid_security for an instrument probes Bloomberg by the
/// instrument's FIGI and lands the rename through reconcile_identity: the
/// dead period closes at as_of, the new one opens, series stay on the same
/// instrument_id.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_invalid_security_run_triggers_a_figi_probe_that_lands_the_rename() {
    let pool = common::pool().await;
    let figi = uniq("BBGAUTO");
    let old_sec = format!("{} US Equity", uniq("DEADT"));
    let new_sec = format!("{} US Equity", uniq("NEWT"));
    let (iid, run_id) = scaffold_dead_run(&pool, Some(&figi), &old_sec).await;

    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": new_sec, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {"ID_BB_GLOBAL": figi, "NAME": "RENAMED CO",
                          "EXCH_CODE": "US", "CRNCY": "USD",
                          "MARKET_SECTOR_DES": "Equity"}}]}]),
        ..Default::default()
    };
    let as_of = chrono::Local::now().date_naive();
    let n = engine::auto_reresolve_invalid(&pool, &mock, run_id, as_of).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(mock.call_count(), 1, "exactly one identity probe");

    let secs: Vec<(String, NaiveDate)> = sqlx::query_as(
        "SELECT value, valid_to FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND system_to = 'infinity' ORDER BY valid_from")
        .bind(iid).fetch_all(&pool).await.unwrap();
    assert_eq!(secs.len(), 2, "rename = two periods: {secs:?}");
    assert_eq!(secs[0].0, old_sec);
    assert_eq!(secs[0].1, as_of, "old period closes at discovery");
    assert_eq!(secs[1].0, new_sec);
}

/// The cooldown: a second run the same week must not spend another 12 hits
/// on the same instrument.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_probed_instrument_is_not_probed_again_within_the_cooldown() {
    let pool = common::pool().await;
    let figi = uniq("BBGCOOL");
    let old_sec = format!("{} US Equity", uniq("DEADC"));
    let (iid, run_id) = scaffold_dead_run(&pool, Some(&figi), &old_sec).await;
    let mock = MockMasterFetcher::default(); // empty identity answer
    let as_of = chrono::Local::now().date_naive();
    engine::auto_reresolve_invalid(&pool, &mock, run_id, as_of).await.unwrap();
    assert_eq!(mock.call_count(), 1);
    // Same instrument, a later run, same week:
    let run2 = insert_invalid_security_run(&pool, iid).await;
    engine::auto_reresolve_invalid(&pool, &mock, run2, as_of).await.unwrap();
    assert_eq!(mock.call_count(), 1, "cooldown must swallow the second probe");
}

/// No FIGI, no probe -- there is nothing stable to ask Bloomberg about.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_instrument_without_a_figi_is_skipped_not_probed() {
    let pool = common::pool().await;
    let old_sec = format!("{} US Equity", uniq("NOFIG"));
    let (_iid, run_id) = scaffold_dead_run(&pool, None, &old_sec).await;
    let mock = MockMasterFetcher::default();
    let n = engine::auto_reresolve_invalid(
        &pool, &mock, run_id, chrono::Local::now().date_naive()).await.unwrap();
    assert_eq!(n, 0);
    assert_eq!(mock.call_count(), 0, "no FIGI means no Bloomberg call at all");
}
