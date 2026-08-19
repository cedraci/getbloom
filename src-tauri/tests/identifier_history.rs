mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::instrument::{history, store::{self, NewAlias}};
use getbloomdata_lib::master_fetch::{HistIdRow, MockMasterFetcher};
use getbloomdata_lib::resolution::engine::{self, Resolution, ResolveInput};
use getbloomdata_lib::resolution::score::Hints;

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

/// A hand-built HISTORICAL_IDS_TIME_RANGE row, for tests that exercise
/// `history::apply`'s ownership logic directly rather than through a
/// captured fixture -- the P0 capture only has the FB/META/METV chain, not
/// every ownership combination this module now has to handle.
fn row(old_id: &str, new_id: &str, date: &str, action_id: Option<&str>) -> HistIdRow {
    HistIdRow {
        date: d(date), old_id: old_id.into(), new_id: new_id.into(),
        old_exch: Some("US".into()), new_exch: Some("US".into()),
        action_id: action_id.map(str::to_string), source: Some("ID Change".into()),
    }
}

/// A bare-ticker alias, the shape a manual/legacy entry takes -- and,
/// notably, the shape `bind_identity` NEVER writes (it only ever writes
/// `bdp_security`/`figi`/`isin`/`bbg_unique`). Kept for the tests that need
/// an instrument owning a ticker without going through the engine at all.
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

/// An instrument bound the way production actually binds one: through
/// `engine::resolve`'s `bloomberg_ref` path, so its identity lives in
/// `bdp_security`/`figi`/`isin` -- never in a bare `ticker` alias. Several
/// tests below exist specifically to prove `history`'s ownership checks work
/// against data shaped like this, not just against a hand-seeded `ticker`
/// row a real bind never produces.
async fn instrument_via_engine(pool: &sqlx::PgPool, ticker: &str, listing: &str) -> i64 {
    let security = format!("{ticker} US Equity");
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": security, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {
                "ID_BB_GLOBAL": uniq("BBG000TEST"), "ID_ISIN": uniq("US0000000000"),
                "EXCH_CODE": "US", "CRNCY": "USD", "CNTRY_ISSUE_ISO": "US",
                "SECURITY_TYP2": "Common Stock", "MARKET_SECTOR_DES": "Equity",
                "NAME": "TEST INC", "LISTING_DATE": listing,
            }}]}]),
        ..Default::default()
    };
    let input = ResolveInput {
        raw: format!("{ticker} US"), yellow_key: "Equity".into(),
        hints: Hints::default(), as_of: d("2026-08-19"), decided_by: "auto".into(),
    };
    let r = engine::resolve(pool, &mock, &input).await.unwrap();
    let Resolution::Bound { instrument_id, method, .. } = r else {
        panic!("expected Bound, got {r:?}")
    };
    assert_eq!(method, "bloomberg_ref");
    instrument_id
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_rename_becomes_two_validity_periods_on_one_instrument() {
    let pool = common::pool().await;
    let meta = uniq("META");
    let fb = uniq("FB");
    // Bound through the engine -- real production shape: a `bdp_security`
    // alias, no `ticker` alias at all.
    let iid = instrument_via_engine(&pool, &meta, "1980-12-12").await;
    let mock = mock_with_tickers(ANCHORED, &[("FB", &fb), ("META", &meta)]);

    let out = history::ingest(&pool, &mock, iid, &format!("{meta} US Equity"), d("1980-01-01"))
        .await.unwrap();
    assert_eq!(out.aliases_added, 1, "FB is added; META is already there");
    assert!(out.links_proposed.is_empty(), "one instrument, no link needed");

    let fb_security = format!("{fb} US Equity");
    let meta_security = format!("{meta} US Equity");

    let aliases = store::aliases(&pool, iid).await.unwrap();
    let fb_ticker = aliases.iter().find(|a| a.id_type == "ticker" && a.value == fb)
        .expect("FB written as a ticker alias");
    assert_eq!(fb_ticker.valid_to, d("2022-06-09"), "FB stopped on the change date");
    assert_eq!(fb_ticker.bbg_action_id.as_deref(), Some("228233742"),
               "Bloomberg's own event id is the key P3 needs for amendments");
    let anchor_str = format!("{meta} US Equity");
    assert_eq!(fb_ticker.anchoring_identifier.as_deref(), Some(anchor_str.as_str()));
    assert_eq!(fb_ticker.source, "bloomberg_hist_ids");
    let fb_sec_alias = aliases.iter().find(|a| a.id_type == "bdp_security" && a.value == fb_security)
        .expect("FB also written as a reconstructed bdp_security -- this is what makes \
                 a user typing \"FB US Equity\" find this instrument");
    assert_eq!(fb_sec_alias.valid_from, d("1980-12-12"));
    assert_eq!(fb_sec_alias.valid_to, d("2022-06-09"));

    // The current identifier's own period was corrected: META did not exist
    // as "META US Equity" at listing, only from the rename onward.
    let meta_sec_alias = aliases.iter().find(|a| a.id_type == "bdp_security" && a.value == meta_security)
        .expect("META's bdp_security alias is still current");
    assert_eq!(meta_sec_alias.valid_from, d("2022-06-09"),
               "corrected away from the listing date once the rename is known");
    assert_eq!(meta_sec_alias.valid_to, store::forever());

    // As-of lookups now tell the two eras apart correctly.
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &fb_security, d("2015-01-01"))
               .await.unwrap(), Some(iid), "FB US Equity resolved FB in 2015");
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &fb_security, d("2026-08-19"))
               .await.unwrap(), None, "FB US Equity resolves nobody today");
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &meta_security, d("2015-01-01"))
               .await.unwrap(), None, "META US Equity did not exist in 2015");
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &meta_security, d("2026-08-19"))
               .await.unwrap(), Some(iid));
}

/// P0 §6.4 as a regression test, against instruments bound the way
/// production actually binds them -- through the engine, so neither carries
/// a bare `ticker` alias at all. Before the fix, `owner_of` only ever
/// searched `ticker` rows, so it could never find either instrument's real
/// identity, and the unanchored META->METV row would have been silently
/// absorbed as an alias of Facebook's instrument. The unanchored answer says
/// META became METV, which is the Roundhill Ball Metaverse ETF, not
/// Facebook. Ingesting it as an alias of this instrument would silently
/// attach another company's identity.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_old_id_belonging_to_another_instrument_proposes_a_link_and_merges_nothing() {
    let pool = common::pool().await;
    let meta = uniq("META");
    let metv = uniq("METV");
    // The METV instrument already exists in the master, under its own
    // engine-bound identity.
    let metv_iid = instrument_via_engine(&pool, &metv, "2021-06-01").await;
    let meta_iid = instrument_via_engine(&pool, &meta, "2012-05-18").await;

    let mock = mock_with_tickers(UNANCHORED, &[("META", &meta), ("METV", &metv)]);
    let out = history::ingest(&pool, &mock, meta_iid, &format!("{meta} US Equity"), d("2000-01-01"))
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

    // Nothing about META's own identity was disturbed by the refused merge.
    assert_eq!(store::find_by_alias(&pool, "bdp_security", &format!("{meta} US Equity"),
               d("2026-08-19")).await.unwrap(), Some(meta_iid));
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
}

/// Important 2: when the New ID is checked first and belongs to someone
/// else, it must not automatically make US the predecessor -- if the Old ID
/// *also* belongs to a (different) other instrument, this row is evidence
/// about those two instruments, not about the one we were asked to ingest
/// for.
#[tokio::test]
#[ignore = "requires postgres"]
async fn both_ends_owned_by_different_others_proposes_between_them_not_us() {
    let pool = common::pool().await;
    let old_tick = uniq("OLDX");
    let new_tick = uniq("NEWX");
    let owner_old = instrument_with_ticker(&pool, &old_tick, "2000-01-01").await;
    let owner_new = instrument_with_ticker(&pool, &new_tick, "2000-01-01").await;
    let unrelated = instrument_with_ticker(&pool, &uniq("SELFX"), "2000-01-01").await;

    let action = uniq("ACT");
    let rows = vec![row(&old_tick, &new_tick, "2010-06-01", Some(&action))];
    let out = history::apply(&pool, unrelated, "TEST Anchor", &rows).await.unwrap();

    assert_eq!(out.aliases_added, 0);
    assert_eq!(out.links_proposed.len(), 1);
    let (pred, succ): (i64, i64) = sqlx::query_as(
        "SELECT predecessor_id, successor_id FROM instrument_link WHERE id = $1")
        .bind(out.links_proposed[0]).fetch_one(&pool).await.unwrap();
    assert_eq!((pred, succ), (owner_old, owner_new),
               "the event is evidence about the other two instruments, not us");
}

/// The degenerate sub-case of Important 2: if the SAME other instrument
/// somehow owns both ends, there is nothing to propose (predecessor ==
/// successor is not even representable -- instrument_link's own CHECK
/// forbids it) -- and this must not error.
#[tokio::test]
#[ignore = "requires postgres"]
async fn both_ends_owned_by_the_same_other_instrument_proposes_nothing() {
    let pool = common::pool().await;
    let old_tick = uniq("SAMEOLD");
    let new_tick = uniq("SAMENEW");
    let owner = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    for ticker in [&old_tick, &new_tick] {
        store::insert_alias(&mut tx, owner.instrument_id, &NewAlias {
            id_type: "ticker".into(), value: ticker.clone(), exch_code: Some("US".into()),
            valid_from: d("2000-01-01"), valid_to: None, source: "user".into(),
            bbg_action_id: None, anchoring_identifier: None,
        }).await.unwrap();
    }
    tx.commit().await.unwrap();
    let unrelated = instrument_with_ticker(&pool, &uniq("SELFY"), "2000-01-01").await;

    let action = uniq("ACT");
    let rows = vec![row(&old_tick, &new_tick, "2010-06-01", Some(&action))];
    let out = history::apply(&pool, unrelated, "TEST Anchor", &rows).await.unwrap();

    assert_eq!(out.aliases_added, 0);
    assert!(out.links_proposed.is_empty(), "degenerate: nothing to propose");
}

/// Important 3: an instrument that already carries the Old ID, open-ended
/// (e.g. a manual entry made before its history was known), must have that
/// period closed when Bloomberg later reports the rename -- not silently
/// discard the event by treating "already ours" as a no-op.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_rename_of_an_identifier_we_already_own_closes_it_instead_of_discarding_it() {
    let pool = common::pool().await;
    let fb = uniq("FBX");
    let meta = uniq("METAX");
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "ticker".into(), value: fb.clone(), exch_code: Some("US".into()),
        valid_from: d("2000-01-01"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();

    let action = uniq("ACT");
    let rows = vec![row(&fb, &meta, "2022-06-09", Some(&action))];
    let out = history::apply(&pool, inst.instrument_id, "TEST Anchor", &rows).await.unwrap();
    assert_eq!(out.aliases_added, 0, "the identifier already existed; closing is not adding");
    assert!(out.links_proposed.is_empty());

    let aliases = store::aliases(&pool, inst.instrument_id).await.unwrap();
    let fb_alias = aliases.iter().find(|a| a.id_type == "ticker" && a.value == fb)
        .expect("FB alias still current, now closed");
    assert_eq!(fb_alias.valid_to, d("2022-06-09"),
               "Bloomberg's rename event closes the period instead of leaving it open forever");

    // Re-applying the same row is a no-op: already closed, nothing to do.
    let again = history::apply(&pool, inst.instrument_id, "TEST Anchor", &rows).await.unwrap();
    assert_eq!(again.aliases_added, 0);
    assert!(again.links_proposed.is_empty());
}

/// M1: `owner_of` must not be an as-of check. An owner whose validity period
/// has not even started yet is still an owner -- the whole reason `owner_of`
/// does not filter by date at all. A row from 2010 must still be refused
/// against an owner whose alias only starts in 2099.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_owner_whose_period_has_not_started_yet_still_counts_as_an_owner() {
    let pool = common::pool().await;
    let ticker = uniq("FUTR");
    let future_owner = instrument_with_ticker(&pool, &ticker, "2099-01-01").await;
    let processing = store::create(&pool).await.unwrap();

    let action = uniq("ACT");
    let rows = vec![row(&uniq("SOMEOLD"), &ticker, "2010-01-01", Some(&action))];
    let out = history::apply(&pool, processing.instrument_id, "TEST Anchor", &rows).await.unwrap();

    assert_eq!(out.aliases_added, 0, "the New ID belongs to someone, even in the future");
    assert_eq!(out.links_proposed.len(), 1);
    let (pred, succ): (i64, i64) = sqlx::query_as(
        "SELECT predecessor_id, successor_id FROM instrument_link WHERE id = $1")
        .bind(out.links_proposed[0]).fetch_one(&pool).await.unwrap();
    assert_eq!((pred, succ), (processing.instrument_id, future_owner));
}

/// Important 5: two live listings can legitimately share a bare ticker
/// across markets (store.rs's own BMW example). `owner_of` must not
/// `LIMIT 1` and silently pick one -- it must recognise the ambiguity and
/// refuse to guess which chain a rename belongs to.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_ticker_owned_by_two_different_instruments_is_left_alone() {
    let pool = common::pool().await;
    let ticker = uniq("BMWX");
    let owner_a = instrument_with_ticker(&pool, &ticker, "2000-01-01").await;
    let owner_b = instrument_with_ticker(&pool, &ticker, "2005-01-01").await;
    let processing = store::create(&pool).await.unwrap();

    let action = uniq("ACT");
    let rows = vec![row(&uniq("SOMEOLD2"), &ticker, "2010-01-01", Some(&action))];
    let out = history::apply(&pool, processing.instrument_id, "TEST Anchor", &rows).await.unwrap();

    assert_eq!(out.aliases_added, 0, "cannot tell which chain we are in");
    assert!(out.links_proposed.is_empty(), "an ambiguous owner proposes nothing");
    let _ = (owner_a, owner_b);
}

/// M4: a row with no Action ID must still be idempotent on re-ingest,
/// falling back to the alias's own natural key (id_type, value, valid_from)
/// -- the same tuple instrument_alias_current uniquely indexes -- instead of
/// attempting a duplicate insert and dying on SQLSTATE 23505.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_row_without_an_action_id_is_still_idempotent_on_reingest() {
    let pool = common::pool().await;
    let old_tick = uniq("NOACT");
    let new_tick = uniq("NOACTNEW");
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: format!("{new_tick} US Equity"),
        exch_code: Some("US".into()), valid_from: d("2000-01-01"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();

    let rows = vec![row(&old_tick, &new_tick, "2010-01-01", None)];
    let first = history::apply(&pool, inst.instrument_id, "TEST Anchor", &rows).await.unwrap();
    assert_eq!(first.aliases_added, 1);
    let second = history::apply(&pool, inst.instrument_id, "TEST Anchor", &rows).await.unwrap();
    assert_eq!(second.aliases_added, 0,
               "no Action ID to key on, but (id_type, value, valid_from) still catches it");
}
