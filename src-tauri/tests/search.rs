mod common;

use common::uniq;
use getbloomdata_lib::instrument::search::{self, Origin};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::resolution::score::Candidate;

fn d(s: &str) -> chrono::NaiveDate { s.parse().unwrap() }

/// Everything this file seeds carries a unique stem (`common::uniq`), and every
/// assertion below checks for the presence/absence/ordering of THIS test's own
/// rows rather than the shape of the whole result set. `tests/` shares one
/// database across parallel threads and across repeated runs with no cleanup,
/// so search -- which queries the whole table by design -- is the most
/// isolation-sensitive module in the plan; a literal "AAPL" would collide with
/// every other test and every previous run.
struct Seeded {
    instrument_id: i64,
    /// The bare ticker stem a user would actually type, e.g. "AAPL<tag>3".
    ticker: String,
    /// The full bdp_security alias, e.g. "AAPL<tag>3 US Equity".
    security: String,
    /// The book_entry label, e.g. "Apple<tag>4".
    label: String,
}

async fn seed(pool: &sqlx::PgPool) -> Seeded {
    let class_name = uniq("Equity");
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1)
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id")
        .bind(&class_name)
        .fetch_one(pool).await.unwrap();

    let inst = store::create(pool).await.unwrap();
    let ticker = uniq("AAPL");
    let security = format!("{ticker} US Equity");
    let label = uniq("Apple");
    let name = uniq("APPLE INC");

    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: security.clone(),
        exch_code: Some("US".into()), valid_from: d("1980-12-12"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", &name,
                    d("1980-12-12"), "bloomberg", None).await.unwrap();
    tx.commit().await.unwrap();

    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(inst.instrument_id).bind(class).bind(&label)
        .execute(pool).await.unwrap();

    Seeded { instrument_id: inst.instrument_id, ticker, security, label }
}

/// The headline requirement: typing AAPL suggests AAPL US Equity, and it costs
/// nothing, because nothing here talks to Bloomberg.
#[tokio::test]
#[ignore = "requires postgres"]
async fn typing_a_ticker_suggests_the_full_security_string() {
    let pool = common::pool().await;
    let s = seed(&pool).await;
    let hits = search::local(&pool, &s.ticker, 10).await.unwrap();
    assert!(hits.iter().any(|h| h.security.as_deref() == Some(s.security.as_str())),
            "got {hits:#?}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_result_says_where_it_came_from() {
    let pool = common::pool().await;
    let s = seed(&pool).await;

    let msft_ticker = uniq("MSFT");
    let msft_security = format!("{msft_ticker} US Equity");
    search::remember_candidates(&pool, &[Candidate {
        security: msft_security.clone(), description: uniq("Microsoft Corp"),
        exchange: Some("US".into()), country: None, currency: None,
        asset_class: None, figi: None }]).await.unwrap();

    let held = search::local(&pool, &s.label, 10).await.unwrap();
    assert_eq!(held[0].origin, Origin::Book, "got {held:#?}");
    assert_eq!(held[0].instrument_id, Some(s.instrument_id));

    let seen = search::local(&pool, &msft_ticker, 10).await.unwrap();
    assert_eq!(seen[0].origin, Origin::Candidate, "got {seen:#?}");
    assert_eq!(seen[0].instrument_id, None, "a cached candidate is not yet an instrument");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_historical_ticker_still_finds_its_instrument() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let old_ticker = uniq("FB");
    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "ticker".into(), value: old_ticker.clone(), exch_code: Some("US".into()),
        valid_from: d("2012-05-18"), valid_to: None, source: "user".into(),
        bbg_action_id: None, anchoring_identifier: None }).await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    tx.commit().await.unwrap();

    let hits = search::local(&pool, &old_ticker, 10).await.unwrap();
    assert!(hits.iter().any(|h| h.instrument_id == Some(inst.instrument_id)),
            "an identifier the instrument used to wear is still how a user looks for it: got {hits:#?}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn results_are_ranked_and_thresholded() {
    let pool = common::pool().await;
    let s = seed(&pool).await;
    let ln_security = format!("{} LN Equity", s.ticker);
    // A row that shares nothing with this test's ticker beyond noise from the
    // shared uniq() tag -- confirmed in psql to sit well under MIN_SIMILARITY.
    let dissimilar_stem = uniq("Nothing Alike");
    let dissimilar_security = format!("{dissimilar_stem} US Equity");
    search::remember_candidates(&pool, &[
        Candidate { security: ln_security.clone(), description: uniq("Apple Inc"),
                    exchange: Some("LN".into()), country: None, currency: None,
                    asset_class: None, figi: None },
        Candidate { security: dissimilar_security.clone(), description: "Nothing alike".into(),
                    exchange: Some("US".into()), country: None, currency: None,
                    asset_class: None, figi: None }]).await.unwrap();

    let hits = search::local(&pool, &s.ticker, 10).await.unwrap();
    assert!(hits.windows(2).all(|w| w[0].similarity >= w[1].similarity),
            "most similar first: got {hits:#?}");
    assert!(hits.iter().all(|h| h.similarity >= search::MIN_SIMILARITY));

    let own: Vec<_> = hits.iter()
        .filter(|h| h.security.as_deref() == Some(s.security.as_str())
                 || h.security.as_deref() == Some(ln_security.as_str()))
        .collect();
    assert_eq!(own.len(), 2, "both of this test's real listings should surface: got {hits:#?}");
    assert!(!hits.iter().any(|h| h.security.as_deref() == Some(dissimilar_security.as_str())),
            "a deliberately dissimilar row of this test's own must not appear: got {hits:#?}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_same_instrument_appears_once_at_its_strongest_origin() {
    let pool = common::pool().await;
    let s = seed(&pool).await;
    // The same security is also in the candidate cache, from an earlier search.
    search::remember_candidates(&pool, &[Candidate {
        security: s.security.clone(), description: uniq("Apple Inc"),
        exchange: Some("US".into()), country: None, currency: None,
        asset_class: None, figi: None }]).await.unwrap();
    let hits = search::local(&pool, &s.security, 10).await.unwrap();
    let for_this: Vec<_> = hits.iter()
        .filter(|h| h.security.as_deref() == Some(s.security.as_str())).collect();
    assert_eq!(for_this.len(), 1, "one row per security, not one per source: got {hits:#?}");
    assert_eq!(for_this[0].origin, Origin::Book);
    assert_eq!(for_this[0].instrument_id, Some(s.instrument_id));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn remembering_a_candidate_twice_refreshes_it_rather_than_duplicating() {
    let pool = common::pool().await;
    let tsla_ticker = uniq("TSLA");
    let tsla_security = format!("{tsla_ticker} US Equity");
    let c = [Candidate { security: tsla_security.clone(), description: uniq("Tesla Inc"),
                         exchange: Some("US".into()), country: None, currency: None,
                         asset_class: None, figi: None }];
    search::remember_candidates(&pool, &c).await.unwrap();
    search::remember_candidates(&pool, &c).await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_candidate WHERE security = $1")
        .bind(&tsla_security)
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
    let (first, last): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT first_seen, last_seen FROM instrument_candidate
                         WHERE security = $1")
        .bind(&tsla_security)
        .fetch_one(&pool).await.unwrap();
    assert!(last >= first);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_empty_query_returns_nothing_rather_than_everything() {
    let pool = common::pool().await;
    seed(&pool).await;
    assert!(search::local(&pool, "   ", 10).await.unwrap().is_empty());
}
