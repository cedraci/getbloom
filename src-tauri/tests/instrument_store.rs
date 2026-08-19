mod common;

use chrono::NaiveDate;
use common::uniq;
use getbloomdata_lib::instrument::store::{self, NewAlias};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

fn ticker(value: &str, from: &str) -> NewAlias {
    NewAlias {
        id_type: "ticker".into(),
        value: value.into(),
        exch_code: Some("US".into()),
        valid_from: d(from),
        valid_to: None,
        source: "user".into(),
        bbg_action_id: None,
        anchoring_identifier: None,
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_ticker_change_produces_two_alias_rows_and_zero_updates_to_value() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let old_val = uniq("FB");
    let new_val = uniq("META");

    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &ticker(&old_val, "2012-05-18"))
        .await.unwrap();
    tx.commit().await.unwrap();

    // The rename: close the old period, open a new one. Never an UPDATE of value.
    let mut tx = pool.begin().await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker(&new_val, "2022-06-09"))
        .await.unwrap();
    tx.commit().await.unwrap();

    let all = store::aliases(&pool, inst.instrument_id).await.unwrap();
    assert_eq!(all.len(), 2, "both identifiers survive");
    let fb = all.iter().find(|a| a.value == old_val).unwrap();
    assert_eq!(fb.valid_to, d("2022-06-09"), "the old ticker is closed, not deleted");
}

/// The sentinel round-trips exactly: an open-ended alias must come back as
/// `store::forever()`, not as whatever Postgres' own DATE 'infinity' decodes
/// to (which would panic -- see the module doc on `forever()`).
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_open_ended_valid_to_round_trips_as_the_forever_sentinel() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let val = uniq("FB");
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker(&val, "2012-05-18"))
        .await.unwrap();
    tx.commit().await.unwrap();

    let all = store::aliases(&pool, inst.instrument_id).await.unwrap();
    assert_eq!(all[0].valid_to, store::forever());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn lookup_is_as_of_a_date_so_the_same_ticker_resolves_differently_over_time() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let old_val = uniq("FB");
    let new_val = uniq("META");
    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &ticker(&old_val, "2012-05-18"))
        .await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker(&new_val, "2022-06-09"))
        .await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(store::find_by_alias(&pool, "ticker", &old_val, d("2015-01-01")).await.unwrap(),
               Some(inst.instrument_id));
    assert_eq!(store::find_by_alias(&pool, "ticker", &old_val, d("2026-01-01")).await.unwrap(),
               None, "FB stopped being this instrument's ticker in 2022");
    assert_eq!(store::find_by_alias(&pool, "ticker", &new_val, d("2026-01-01")).await.unwrap(),
               Some(inst.instrument_id));
}

/// Two live instruments can legitimately wear the same bare ticker at once
/// (e.g. the same symbol on two exchanges). `find_all_by_alias` must surface
/// both; `find_by_alias` must refuse to guess and return `None` rather than
/// silently pick one.
#[tokio::test]
#[ignore = "requires postgres"]
async fn find_by_alias_refuses_to_pick_between_two_overlapping_instruments() {
    let pool = common::pool().await;
    let frankfurt = store::create(&pool).await.unwrap();
    let new_york = store::create(&pool).await.unwrap();
    let val = uniq("BMW");
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, frankfurt.instrument_id, &ticker(&val, "2000-01-01"))
        .await.unwrap();
    store::insert_alias(&mut tx, new_york.instrument_id, &ticker(&val, "2000-01-01"))
        .await.unwrap();
    tx.commit().await.unwrap();

    let mut all = store::find_all_by_alias(&pool, "ticker", &val, d("2026-01-01")).await.unwrap();
    all.sort();
    let mut expected = vec![frankfurt.instrument_id, new_york.instrument_id];
    expected.sort();
    assert_eq!(all, expected, "both overlapping instruments are surfaced");

    assert_eq!(store::find_by_alias(&pool, "ticker", &val, d("2026-01-01")).await.unwrap(), None,
               "an ambiguous match must not be silently resolved");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn lookup_is_case_insensitive() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let val = uniq("AAPL US");
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker(&val, "1980-12-12"))
        .await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(store::find_by_alias(&pool, "ticker", &val.to_lowercase(), d("2026-01-01")).await.unwrap(),
               Some(inst.instrument_id));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_correction_supersedes_rather_than_erases() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let wrong_val = uniq("APPL US");
    let right_val = uniq("AAPL US");
    let mut tx = pool.begin().await.unwrap();
    let wrong = store::insert_alias(&mut tx, inst.instrument_id, &ticker(&wrong_val, "1980-12-12"))
        .await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    store::supersede_alias(&mut tx, wrong).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker(&right_val, "1980-12-12"))
        .await.unwrap();
    tx.commit().await.unwrap();

    // aliases() returns only what we currently believe...
    let current = store::aliases(&pool, inst.instrument_id).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].value, right_val);
    // ...but the mistaken row is still on disk, which is what point-in-time needs.
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_alias WHERE instrument_id = $1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(total, 2);
}

/// A superseded alias is no longer current, so closing its `valid_to` is not
/// this call's decision to make -- the guard on `close_alias` must make this
/// a no-op rather than resurrecting a corrected-away row's real-world period.
#[tokio::test]
#[ignore = "requires postgres"]
async fn closing_an_already_superseded_alias_is_a_no_op() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let val = uniq("APPL US");
    let mut tx = pool.begin().await.unwrap();
    let wrong = store::insert_alias(&mut tx, inst.instrument_id, &ticker(&val, "1980-12-12"))
        .await.unwrap();
    store::supersede_alias(&mut tx, wrong).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    store::close_alias(&mut tx, wrong, d("2000-01-01")).await.unwrap();
    tx.commit().await.unwrap();

    let valid_to: NaiveDate = sqlx::query_scalar(
        "SELECT valid_to FROM instrument_alias WHERE id = $1")
        .bind(wrong).fetch_one(&pool).await.unwrap();
    assert_eq!(valid_to, store::forever(), "a superseded row's valid_to must not move");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn setting_an_attribute_twice_for_the_same_period_supersedes_the_first() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "FACEBOOK INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "META PLATFORMS INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    tx.commit().await.unwrap();

    let now = store::attrs(&pool, inst.instrument_id, d("2026-01-01")).await.unwrap();
    let names: Vec<&str> = now.iter().filter(|a| a.attr == "name")
        .map(|a| a.value.as_str()).collect();
    assert_eq!(names, ["META PLATFORMS INC"], "one current value per attribute period");
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_attr WHERE instrument_id = $1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(total, 2, "the earlier belief is retained beneath");
}

/// A rename (different valid_from, different value) must close the prior
/// period's valid_to at the new period's start, not leave both rows open --
/// otherwise a caller reading `attrs()` at a late date gets two "name" rows
/// with no principled tiebreak.
#[tokio::test]
#[ignore = "requires postgres"]
async fn setting_an_attribute_for_a_new_period_closes_the_prior_one() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "FACEBOOK INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "META PLATFORMS INC",
                    d("2022-06-09"), "bloomberg", None).await.unwrap();
    tx.commit().await.unwrap();

    let late = store::attrs(&pool, inst.instrument_id, d("2026-01-01")).await.unwrap();
    let late_names: Vec<&str> = late.iter().filter(|a| a.attr == "name")
        .map(|a| a.value.as_str()).collect();
    assert_eq!(late_names, ["META PLATFORMS INC"]);

    let early = store::attrs(&pool, inst.instrument_id, d("2015-01-01")).await.unwrap();
    let early_names: Vec<&str> = early.iter().filter(|a| a.attr == "name")
        .map(|a| a.value.as_str()).collect();
    assert_eq!(early_names, ["FACEBOOK INC"], "the earlier period is still readable as-of its own time");
}

/// Writing the same value again for the same period is a no-op: it must not
/// add a duplicate row to history.
#[tokio::test]
#[ignore = "requires postgres"]
async fn setting_an_identical_attribute_value_for_the_same_period_is_a_no_op() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "FACEBOOK INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "FACEBOOK INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    tx.commit().await.unwrap();

    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_attr WHERE instrument_id = $1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(total, 1, "an identical re-assertion must not write a second row");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_current_security_string_is_derived_not_stored() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let old_val = uniq("FB US Equity");
    let new_val = uniq("META US Equity");
    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: old_val.clone(),
        ..ticker(&old_val, "2012-05-18") }).await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: new_val.clone(),
        ..ticker(&new_val, "2022-06-09") }).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(store::current_security(&pool, inst.instrument_id, d("2015-01-01"))
                   .await.unwrap(), Some(old_val));
    assert_eq!(store::current_security(&pool, inst.instrument_id, d("2026-08-19"))
                   .await.unwrap(), Some(new_val));
}

/// P0 7.2: no Bloomberg field returns a successor, so a link is always a
/// derived proposal. Until a human confirms it, nothing may follow it.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_unconfirmed_link_is_not_followed() {
    let pool = common::pool().await;
    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let link = store::propose_link(&pool, a.instrument_id, b.instrument_id, "rename",
                                   d("2022-06-09"), serde_json::json!({"source": "test"}))
        .await.unwrap();
    assert_eq!(store::confirmed_successors(&pool, a.instrument_id).await.unwrap(), Vec::<i64>::new(),
               "a proposal is not a fact");
    store::confirm_link(&pool, link, "laurent").await.unwrap();
    assert_eq!(store::confirmed_successors(&pool, a.instrument_id).await.unwrap(),
               vec![b.instrument_id]);
}

/// A spinoff legitimately produces more than one confirmed successor; a
/// `LIMIT 1` implementation would silently drop one of them.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_predecessor_can_have_more_than_one_confirmed_successor() {
    let pool = common::pool().await;
    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let c = store::create(&pool).await.unwrap();
    let link1 = store::propose_link(&pool, a.instrument_id, b.instrument_id, "spinoff",
                                    d("2022-06-09"), serde_json::json!({"source": "test"}))
        .await.unwrap();
    let link2 = store::propose_link(&pool, a.instrument_id, c.instrument_id, "spinoff",
                                    d("2022-06-09"), serde_json::json!({"source": "test"}))
        .await.unwrap();
    store::confirm_link(&pool, link1, "laurent").await.unwrap();
    store::confirm_link(&pool, link2, "laurent").await.unwrap();

    let mut successors = store::confirmed_successors(&pool, a.instrument_id).await.unwrap();
    successors.sort();
    let mut expected = vec![b.instrument_id, c.instrument_id];
    expected.sort();
    assert_eq!(successors, expected, "both confirmed successors survive");
}

/// A link cannot be re-confirmed: a second confirm_link call must not
/// silently overwrite who confirmed it.
#[tokio::test]
#[ignore = "requires postgres"]
async fn confirming_an_already_confirmed_link_does_not_overwrite_the_confirmer() {
    let pool = common::pool().await;
    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let link = store::propose_link(&pool, a.instrument_id, b.instrument_id, "rename",
                                   d("2022-06-09"), serde_json::json!({"source": "test"}))
        .await.unwrap();
    store::confirm_link(&pool, link, "laurent").await.unwrap();
    store::confirm_link(&pool, link, "someone_else").await.unwrap();
    let by: String = sqlx::query_scalar(
        "SELECT confirmed_by FROM instrument_link WHERE id = $1")
        .bind(link).fetch_one(&pool).await.unwrap();
    assert_eq!(by, "laurent", "the second confirm must be a no-op");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn bloomberg_ids_can_be_filled_once_and_never_changed() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    // id_bb_global is UNIQUE across all instruments, and this database is never
    // reset between runs, so the FIGI itself must be routed through uniq() too.
    let figi = uniq("BBG000B9XRY4");
    let other_figi = uniq("BBG000000000");
    store::set_bloomberg_ids(&pool, inst.instrument_id, Some(&figi), None)
        .await.expect("null -> value");
    let err = store::set_bloomberg_ids(&pool, inst.instrument_id, Some(&other_figi), None)
        .await.unwrap_err();
    assert!(err.to_string().contains("write-once"), "got: {err}");
}
