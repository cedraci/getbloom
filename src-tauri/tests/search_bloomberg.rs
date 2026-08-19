mod common;

use common::uniq;
use getbloomdata_lib::instrument::search;
use getbloomdata_lib::master_fetch::MockMasterFetcher;

/// `tests/` shares one database across parallel threads and across repeated
/// runs with no cleanup. `instrument_candidate.security` is TEXT UNIQUE, so
/// every security this mock returns must carry a unique stem -- a literal
/// "AAPL US<equity>" would collide with every other run of this suite.
fn mock(stem: &str) -> MockMasterFetcher {
    let a = format!("{stem} US<equity>");
    let b = format!("{stem} LN<equity>");
    let c = format!("{stem} US 08/21/26 C400<equity>");
    MockMasterFetcher {
        list_raw: serde_json::json!([{"results": [
            {"security": a, "description": "Apple Inc"},
            {"security": b, "description": "Apple Inc"},
            {"security": c, "description": "Apple call"}]}]),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_bloomberg_search_caches_every_result_permanently() {
    let pool = common::pool().await;
    let stem = uniq("AAPL");
    let m = mock(&stem);
    let out = search::bloomberg(&pool, &m, &stem, "Equity").await.unwrap();
    assert_eq!(m.call_count(), 1, "exactly one instrumentListRequest");
    assert!(out.cached >= 2);

    // The point of the cache: the same search now needs no call at all.
    let local = search::local(&pool, &stem, 10).await.unwrap();
    assert!(local.iter().any(|h| h.security.as_deref() == Some(&format!("{stem} US Equity"))));
    assert!(local.iter().any(|h| h.security.as_deref() == Some(&format!("{stem} LN Equity"))));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_raw_bloomberg_form_is_never_stored_as_a_security_string() {
    let pool = common::pool().await;
    let stem = uniq("AAPL");
    search::bloomberg(&pool, &mock(&stem), &stem, "Equity").await.unwrap();
    let bad: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_candidate WHERE security LIKE $1 AND security LIKE '%<%'")
        .bind(format!("{stem}%"))
        .fetch_one(&pool).await.unwrap();
    assert_eq!(bad, 0, "pasting the raw form produces exactly the malformed \
                        identifier migration 0004 had to repair");
}

/// The ledger write moved OUT of this module and into the wire seam
/// (`BlpapiMasterFetcher`), where a future call site cannot forget it -- four
/// call sites in `resolution` already had. So what this test pins changed with
/// it, and both halves matter:
///
///   * `search::bloomberg` itself writes nothing to `hit_ledger`. If it still
///     did, every real search would be charged TWICE, once here and once at
///     the seam.
///   * it still reports the charge to its caller, because the UI shows the
///     estimated cost of the button the user just pressed.
///
/// The accounting the seam actually applies is pinned by
/// `master_fetch`'s `the_wire_seam_charges_one_hit_per_security_field_pair`;
/// it cannot be exercised here because `MockMasterFetcher` has no pool and
/// deliberately records nothing, which is what keeps every other test's
/// ledger assertions honest.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_search_module_no_longer_writes_the_ledger_itself() {
    let pool = common::pool().await;
    let stem = uniq("AAPL");
    // Scope to rows THIS test creates: a global sum/count over hit_ledger is
    // a race under parallel execution (a book_entry count once came back 45
    // vs 44 this way), so capture the high-water mark before the call and
    // assert only on ids above it.
    let before_id: i64 = sqlx::query_scalar("SELECT coalesce(max(id),0) FROM hit_ledger")
        .fetch_one(&pool).await.unwrap();

    let out = search::bloomberg(&pool, &mock(&stem), &stem, "Equity").await.unwrap();

    let new_rows: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM hit_ledger WHERE id > $1 AND purpose = 'search'")
        .bind(before_id)
        .fetch_one(&pool).await.unwrap();
    assert_eq!(new_rows, 0,
               "the ledger is charged at the wire seam; a second write here \
                would double-count every real search");
    assert_eq!(out.estimated_hits, getbloomdata_lib::budget::SEARCH_HIT_COST,
               "the caller is still told what the button cost");
    assert!(out.estimated_hits > 0, "whether instrumentListRequest is metered is \
                                     unknown (spec §10 q2), so it is counted");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_empty_query_never_reaches_bloomberg() {
    let pool = common::pool().await;
    let stem = uniq("AAPL");
    let m = mock(&stem);
    let out = search::bloomberg(&pool, &m, "   ", "Equity").await.unwrap();
    assert_eq!(m.call_count(), 0);
    assert!(out.hits.is_empty());
    assert_eq!(out.estimated_hits, 0);
}
