mod common;

use common::uniq;
use getbloomdata_lib::book::{self, AddOutcome, AddToBook};
use getbloomdata_lib::master_fetch::MockMasterFetcher;
use getbloomdata_lib::resolution::score::Hints;

async fn equity_class(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("Equity"))
        .fetch_one(pool).await.unwrap()
}

fn req(raw: &str, class: i64) -> AddToBook {
    AddToBook { raw: raw.into(), yellow_key: "Equity".into(), asset_class_id: class,
                label: raw.into(), hints: Hints::default() }
}

fn identity_mock(security: &str, figi: &str) -> MockMasterFetcher {
    MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": security, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {"ID_BB_GLOBAL": figi, "EXCH_CODE": "US", "CRNCY": "USD",
                          "NAME": "TEST INC", "LISTING_DATE": "2000-01-03"}}]}]),
        ..Default::default()
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn adding_an_entry_resolves_it_and_derives_its_security_string() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let ticker = uniq("ZDDD");
    let figi = uniq("BBG000TESTD1");
    let security = format!("{ticker} US Equity");
    let mock = identity_mock(&security, &figi);
    let out = book::add(&pool, &mock, &req(&format!("{ticker} US"), class), "laurent")
        .await.unwrap();
    let AddOutcome::Added(entry) = out else { panic!("expected Added, got {out:?}") };
    assert_eq!(entry.security.as_deref(), Some(security.as_str()),
               "the security string is derived from the alias, not stored on the entry");
    assert!(!entry.review_pending);
}

/// The constraint that replaced UNIQUE (bdp_security): one entry per instrument.
#[tokio::test]
#[ignore = "requires postgres"]
async fn the_same_instrument_cannot_be_added_to_the_book_twice() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let ticker = uniq("ZEEE");
    let figi = uniq("BBG000TESTE1");
    let mock = identity_mock(&format!("{ticker} US Equity"), &figi);
    book::add(&pool, &mock, &req(&format!("{ticker} US"), class), "laurent").await.unwrap();
    // Second add resolves locally to the same instrument -- and must not create
    // a second row, nor fail with a confusing constraint error.
    let out = book::add(&pool, &mock, &req(&format!("{ticker} US"), class), "laurent")
        .await.unwrap();
    let AddOutcome::Added(entry) = out else { panic!("expected Added") };
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM book_entry WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_ambiguous_addition_creates_a_review_and_no_book_entry() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let ticker = uniq("ZFFF");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": format!("{ticker} US<equity>"), "description": "Test"},
            {"security": format!("{ticker} LN<equity>"), "description": "Test"}]}]),
        ..Default::default()
    };
    // Scoped as a before/after delta rather than a global count -- other tests
    // share this database and never clean up, so an absolute zero would be
    // false the moment any other test has run.
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM book_entry")
        .fetch_one(&pool).await.unwrap();
    let out = book::add(&pool, &mock, &req(&ticker, class), "laurent").await.unwrap();
    assert!(matches!(out, AddOutcome::NeedsReview { .. }), "got {out:?}");
    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM book_entry")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(after, before, "an unresolved identifier must not quietly enter the book");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn deactivating_an_entry_keeps_its_instrument_and_its_history() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let ticker = uniq("ZGGG");
    let figi = uniq("BBG000TESTG1");
    let mock = identity_mock(&format!("{ticker} US Equity"), &figi);
    let AddOutcome::Added(e) = book::add(&pool, &mock, &req(&format!("{ticker} US"), class),
        "laurent").await.unwrap() else { panic!() };
    book::set_active(&pool, e.instrument_id, false).await.unwrap();
    let listed = book::list(&pool).await.unwrap();
    let found = listed.iter().find(|b| b.instrument_id == e.instrument_id).unwrap();
    assert!(!found.active, "still listed, just inactive");
    let aliases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_alias WHERE instrument_id = $1")
        .bind(e.instrument_id).fetch_one(&pool).await.unwrap();
    assert!(aliases > 0);
}
