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
    // qualifier), the step-4 search, the step-5 confirming reference call
    // for the scored winner, and the one-per-instrument anchored identifier
    // history request Task 8 fires after a successful bind. Nothing here
    // should be able to grow this silently -- e.g. a second identity() call
    // snuck into scoring.
    assert_eq!(fetcher.call_count(), 4,
               "1 failed reference probe + 1 search + 1 confirming reference call \
                + 1 identifier history request");
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
    let iid = engine::resolve_review(&pool, &resolve_mock, review_id, &chosen, "laurent")
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
    let iid = engine::resolve_review(&pool, &list_mock, review_id, &chosen, "laurent")
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
    engine::resolve_review(&pool, &mock, review_id, &chosen, "laurent").await.unwrap();

    let err = engine::resolve_review(&pool, &mock, review_id, &chosen, "laurent").await
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
    let err = engine::resolve_review(&pool, &mock, review_id, &chosen, "laurent").await
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
    let iid1 = engine::resolve_review(&pool, &mock, review1, &chosen, "laurent").await.unwrap();

    // A second, independent search for the same raw identifier -- the bound
    // alias from the first review does not match this input's own probes
    // (it was built from the bare ticker, not "<ticker> US Equity"), so this
    // also reaches step 4 and opens a second review.
    let r2 = engine::resolve(&pool, &mock, &input(&ticker)).await.unwrap();
    let Resolution::NeedsReview { review_id: review2, .. } = r2 else { panic!("expected review") };
    assert_ne!(review1, review2);
    let iid2 = engine::resolve_review(&pool, &mock, review2, &chosen, "laurent").await.unwrap();

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
    let iid = engine::resolve_review(&pool, &resolve_mock, review_id, &chosen, "laurent")
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
