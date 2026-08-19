mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::instrument::{history, store::{self, NewAlias}};
use getbloomdata_lib::master_fetch::MockMasterFetcher;

const HISTIDS: &str = include_str!(
    "../../docs/superpowers/specs/blpapi-facts/histids_report.json");

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

fn capture(key: &str) -> serde_json::Value {
    let all: serde_json::Value = serde_json::from_str(HISTIDS).unwrap();
    all[key].clone()
}

const ANCHORED: &str = "META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', \
                        'HISTORICAL_STARTING_IDENTIFIER']";
const UNANCHORED: &str = "META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT']";

/// The fixture's `Old ID`/`New ID` values ("FB", "META", "METV") are the exact
/// strings Bloomberg returned for the P0 capture, and `history::apply` reads
/// them straight off the row -- renaming them in the JSON would lose the point
/// of the regression test. But `tests/` shares one database across parallel
/// threads and repeated runs with no cleanup, and `owner_of` looks a ticker up
/// by value alone, with no notion of "this test's own rows". Reusing the
/// literal fixture tickers across repeated runs would let a *previous* run's
/// leftover "META" alias answer `owner_of` instead of this run's own,
/// producing a different (wrong) outcome the second time the suite runs.
///
/// Substituting a `uniq()`-tagged ticker for each fixture token, everywhere it
/// appears in the row, keeps the row's shape intact -- a rename from one
/// ticker to another, an anchored answer vs. an unanchored one naming a
/// different company -- while making every ticker this test touches unique to
/// this run. `subs` pairs are matched as exact quoted JSON string tokens, so
/// substituting "META" can never accidentally touch "METV" or the unrelated
/// "security": "META US Equity" field (a different, longer quoted string).
fn mock_with_tickers(key: &str, subs: &[(&str, &str)]) -> MockMasterFetcher {
    let mut text = capture(key).to_string();
    for (from, to) in subs {
        text = text.replace(&format!("\"{from}\""), &format!("\"{to}\""));
    }
    let raw: serde_json::Value = serde_json::from_str(&text).unwrap();
    MockMasterFetcher { hist_ids_raw: raw, ..Default::default() }
}

async fn instrument_with_ticker(pool: &sqlx::PgPool, ticker: &str, from: &str) -> i64 {
    let inst = store::create(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "ticker".into(), value: ticker.into(), exch_code: Some("US".into()),
        valid_from: d(from), valid_to: None, source: "user".into(),
        bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    inst.instrument_id
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_rename_becomes_two_validity_periods_on_one_instrument() {
    let pool = common::pool().await;
    let meta = uniq("META");
    let fb = uniq("FB");
    let iid = instrument_with_ticker(&pool, &meta, "2022-06-09").await;
    let mock = mock_with_tickers(ANCHORED, &[("FB", &fb), ("META", &meta)]);

    let out = history::ingest(&pool, &mock, iid, "META US Equity", d("2000-01-01"))
        .await.unwrap();
    assert_eq!(out.aliases_added, 1, "FB is added; META is already there");
    assert!(out.links_proposed.is_empty(), "one instrument, no link needed");

    let aliases = store::aliases(&pool, iid).await.unwrap();
    let fb_alias = aliases.iter().find(|a| a.value == fb).expect("FB alias");
    assert_eq!(fb_alias.valid_to, d("2022-06-09"), "FB stopped on the change date");
    assert_eq!(fb_alias.bbg_action_id.as_deref(), Some("228233742"),
               "Bloomberg's own event id is the key P3 needs for amendments");
    assert_eq!(fb_alias.anchoring_identifier.as_deref(), Some("META US Equity"));
    assert_eq!(fb_alias.source, "bloomberg_hist_ids");
}

/// P0 §6.4 as a regression test. The unanchored answer says META became METV,
/// which is the Roundhill Ball Metaverse ETF, not Facebook. Ingesting it as an
/// alias of this instrument would silently attach another company's identity.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_old_id_belonging_to_another_instrument_proposes_a_link_and_merges_nothing() {
    let pool = common::pool().await;
    let meta = uniq("META");
    let metv = uniq("METV");
    // The METV instrument already exists in the master, under its own identity.
    let metv_iid = instrument_with_ticker(&pool, &metv, "2022-01-31").await;
    let meta_iid = instrument_with_ticker(&pool, &meta, "2022-06-09").await;

    let mock = mock_with_tickers(UNANCHORED, &[("META", &meta), ("METV", &metv)]);
    let out = history::ingest(&pool, &mock, meta_iid, "META US Equity", d("2000-01-01"))
        .await.unwrap();

    assert_eq!(out.aliases_added, 0,
               "an identifier owned by another instrument is never absorbed");
    assert_eq!(out.links_proposed.len(), 1);

    let (pred, succ, confirmed): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT predecessor_id, successor_id, confirmed_by FROM instrument_link
          WHERE id = $1").bind(out.links_proposed[0])
        .fetch_one(&pool).await.unwrap();
    assert_eq!((pred, succ), (meta_iid, metv_iid));
    assert_eq!(confirmed, None, "it is a proposal until a human agrees");
    assert!(store::confirmed_successors(&pool, meta_iid).await.unwrap().is_empty(),
            "an unconfirmed link is not followed");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn ingestion_without_an_anchor_is_refused_before_any_request_is_sent() {
    let pool = common::pool().await;
    let meta = uniq("META");
    let fb = uniq("FB");
    let iid = instrument_with_ticker(&pool, &meta, "2022-06-09").await;
    let mock = mock_with_tickers(ANCHORED, &[("FB", &fb), ("META", &meta)]);
    let err = history::ingest(&pool, &mock, iid, "  ", d("2000-01-01")).await.unwrap_err();
    assert!(err.to_string().contains("anchoring"), "got: {err}");
    assert_eq!(mock.call_count(), 0, "a refused request must not cost a hit");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn ingesting_twice_adds_nothing_the_second_time() {
    let pool = common::pool().await;
    let meta = uniq("META");
    let fb = uniq("FB");
    let iid = instrument_with_ticker(&pool, &meta, "2022-06-09").await;
    let mock = mock_with_tickers(ANCHORED, &[("FB", &fb), ("META", &meta)]);
    history::ingest(&pool, &mock, iid, "META US Equity", d("2000-01-01")).await.unwrap();
    let second = history::ingest(&pool, &mock, iid, "META US Equity", d("2000-01-01"))
        .await.unwrap();
    assert_eq!(second.aliases_added, 0, "the same Action ID is not applied twice");
    assert_eq!(store::aliases(&pool, iid).await.unwrap().len(), 2,
               "scoped to this test's own instrument");
}
