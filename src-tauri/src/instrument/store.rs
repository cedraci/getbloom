//! The only supported way to write identity.
//!
//! Every change is close-and-insert. There is no update path for a value, and
//! the database enforces that independently (see migration 0001's triggers) so
//! that a mistake here fails loudly rather than quietly rewriting history.
//!
//! Two time axes:
//!   valid_from/valid_to   when the fact was true in the world
//!   system_from/system_to when we believed it
//! Closing valid_to records a real-world change (a ticker was renamed).
//! Closing system_to records a correction (we had it wrong).

use crate::error::AppResult;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

pub type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Instrument {
    pub instrument_id: i64,
    pub id_bb_global: Option<String>,
    pub id_bb_unique: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Alias {
    pub id: i64,
    pub instrument_id: i64,
    pub id_type: String,
    pub value: String,
    pub exch_code: Option<String>,
    pub valid_from: NaiveDate,
    pub valid_to: NaiveDate,
    pub source: String,
    pub bbg_action_id: Option<String>,
    pub anchoring_identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Attr {
    pub id: i64,
    pub instrument_id: i64,
    pub attr: String,
    pub value: String,
    pub valid_from: NaiveDate,
    pub valid_to: NaiveDate,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct NewAlias {
    pub id_type: String,
    pub value: String,
    pub exch_code: Option<String>,
    pub valid_from: NaiveDate,
    /// None means open-ended.
    pub valid_to: Option<NaiveDate>,
    pub source: String,
    pub bbg_action_id: Option<String>,
    /// REQUIRED when source is 'bloomberg_hist_ids'; the database refuses
    /// otherwise. See P0 6.4.
    pub anchoring_identifier: Option<String>,
}

/// Stand-in for "open-ended" on the Rust side.
///
/// Postgres' DATE 'infinity' is not a date chrono can represent: sqlx decodes a
/// binary DATE by adding the day count to 2000-01-01, and Postgres encodes
/// 'infinity' as i32::MAX days -- about 5.9 million years -- which overflows
/// chrono::NaiveDate's range and panics on decode. Nor can `NaiveDate::MAX`
/// (262142-12-31) stand in for it: chrono serializes an out-of-range year with
/// an ISO expanded sign ("+262142-12-31"), which is `Invalid Date` in
/// JavaScript at the frontend boundary. 9999-12-31 is a real, finite,
/// four-digit date far enough out to behave as "forever" for every comparison
/// this module does, and it is what migration 0001's `valid_to` default and
/// `..._no_infinity` CHECK constraints standardize on -- so this value must
/// match the migration exactly. This module never writes the literal SQL
/// 'infinity' into a column it later decodes as NaiveDate (valid_to on
/// alias/attr rows): every open-ended insert below binds `forever()`
/// explicitly. `system_to` (a TIMESTAMPTZ this module never reads back into
/// Rust) is unaffected and stays at real 'infinity'.
pub fn forever() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()
}

pub async fn create(pool: &PgPool) -> AppResult<Instrument> {
    let mut tx = pool.begin().await?;
    let inst = create_tx(&mut tx).await?;
    tx.commit().await?;
    Ok(inst)
}

/// Same as `create`, but inside a transaction the caller controls -- for a
/// caller (resolution::engine::bind_identity) that must commit the new
/// instrument together with its identity or not at all.
pub async fn create_tx(tx: &mut Tx<'_>) -> AppResult<Instrument> {
    Ok(sqlx::query_as::<_, Instrument>(
        "INSERT INTO instrument DEFAULT VALUES
         RETURNING instrument_id, id_bb_global, id_bb_unique")
        .fetch_one(&mut **tx).await?)
}

/// Fill the Bloomberg identifiers once they are known. The trigger refuses any
/// attempt to change a value that is already set.
pub async fn set_bloomberg_ids(pool: &PgPool, instrument_id: i64,
                               figi: Option<&str>, bbg_unique: Option<&str>)
    -> AppResult<()>
{
    let mut tx = pool.begin().await?;
    set_bloomberg_ids_tx(&mut tx, instrument_id, figi, bbg_unique).await?;
    tx.commit().await?;
    Ok(())
}

/// Same as `set_bloomberg_ids`, but inside a transaction the caller controls.
pub async fn set_bloomberg_ids_tx(tx: &mut Tx<'_>, instrument_id: i64,
                                  figi: Option<&str>, bbg_unique: Option<&str>)
    -> AppResult<()>
{
    sqlx::query(
        "UPDATE instrument
            SET id_bb_global = COALESCE($2, id_bb_global),
                id_bb_unique = COALESCE($3, id_bb_unique)
          WHERE instrument_id = $1")
        .bind(instrument_id).bind(figi).bind(bbg_unique)
        .execute(&mut **tx).await?;
    Ok(())
}

pub async fn insert_alias(tx: &mut Tx<'_>, instrument_id: i64, new: &NewAlias)
    -> AppResult<i64>
{
    let valid_to = new.valid_to.unwrap_or_else(forever);
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, exch_code, valid_from, valid_to,
            source, bbg_action_id, anchoring_identifier)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)
         RETURNING id")
        .bind(instrument_id).bind(&new.id_type).bind(&new.value)
        .bind(&new.exch_code).bind(new.valid_from).bind(valid_to)
        .bind(&new.source).bind(&new.bbg_action_id).bind(&new.anchoring_identifier)
        .fetch_one(&mut **tx).await?;
    Ok(id)
}

/// The identifier stopped being true in the world on `valid_to`.
/// A no-op against a row that has already been superseded (system_to closed):
/// that row is no longer current, so its real-world end date is not this
/// call's to decide.
pub async fn close_alias(tx: &mut Tx<'_>, alias_id: i64, valid_to: NaiveDate)
    -> AppResult<()>
{
    sqlx::query("UPDATE instrument_alias SET valid_to = $2
                  WHERE id = $1 AND system_to = 'infinity'")
        .bind(alias_id).bind(valid_to).execute(&mut **tx).await?;
    Ok(())
}

/// We were wrong about the identifier. The row stays on disk; it stops being
/// current. This is what makes a point-in-time read of a past belief possible.
pub async fn supersede_alias(tx: &mut Tx<'_>, alias_id: i64) -> AppResult<()> {
    sqlx::query("UPDATE instrument_alias SET system_to = now()
                  WHERE id = $1 AND system_to = 'infinity'")
        .bind(alias_id).execute(&mut **tx).await?;
    Ok(())
}

/// Every instrument that wore this identifier on this date, as best we know
/// today. Ordinarily this has zero or one entries, but genuine overlap is
/// real -- two live listings can legitimately both wear ticker "BMW" in
/// different markets at once -- and silently picking one of them is exactly
/// the failure this project exists to prevent. Callers that need to resolve
/// (not just detect) an overlap use this directly; Task 7 routes more than
/// one match to a human review queue.
pub async fn find_all_by_alias(pool: &PgPool, id_type: &str, value: &str, as_of: NaiveDate)
    -> AppResult<Vec<i64>>
{
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT instrument_id FROM instrument_alias
          WHERE id_type = $1 AND lower(value) = lower($2)
            AND valid_from <= $3 AND valid_to > $3
            AND system_to = 'infinity'
          ORDER BY instrument_id")
        .bind(id_type).bind(value).bind(as_of)
        .fetch_all(pool).await?)
}

/// Which single instrument wore this identifier on this date. `None` means
/// either nobody did, or -- see `find_all_by_alias` -- more than one did;
/// this function cannot and must not guess between "absent" and "ambiguous".
/// A caller that needs to tell those apart uses `find_all_by_alias` instead.
pub async fn find_by_alias(pool: &PgPool, id_type: &str, value: &str, as_of: NaiveDate)
    -> AppResult<Option<i64>>
{
    let mut matches = find_all_by_alias(pool, id_type, value, as_of).await?;
    Ok(match matches.len() {
        1 => matches.pop(),
        _ => None,
    })
}

pub async fn aliases(pool: &PgPool, instrument_id: i64) -> AppResult<Vec<Alias>> {
    Ok(sqlx::query_as::<_, Alias>(
        "SELECT id, instrument_id, id_type, value, exch_code, valid_from, valid_to,
                source, bbg_action_id, anchoring_identifier
           FROM instrument_alias
          WHERE instrument_id = $1 AND system_to = 'infinity'
          ORDER BY valid_from, id_type")
        .bind(instrument_id).fetch_all(pool).await?)
}

/// The security string to send to Bloomberg for this instrument on this date.
/// Derived from the alias valid then -- never stored on the book entry, because
/// one instrument wears several security strings over its life.
pub async fn current_security(pool: &PgPool, instrument_id: i64, as_of: NaiveDate)
    -> AppResult<Option<String>>
{
    Ok(sqlx::query_scalar(
        "SELECT value FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND valid_from <= $2 AND valid_to > $2
            AND system_to = 'infinity'
          ORDER BY valid_from DESC LIMIT 1")
        .bind(instrument_id).bind(as_of)
        .fetch_optional(pool).await?)
}

/// Record an attribute for a validity period.
///
/// Two different things can be true when this is called, and they must be
/// told apart by whether a row already exists for this EXACT `valid_from`,
/// not by call order (Task 7 can resolve the same instrument twice with
/// `valid_from` derived from a listing date that starts absent and later
/// appears, producing calls in either chronological order):
///   - a correction: a row for this exact `valid_from` already exists. We
///     already asserted something for this period and it was wrong --
///     supersede it (`system_to`) and insert the fix, inheriting the
///     superseded row's `valid_to` unchanged. Never move a boundary here --
///     if the new row instead took `forever()`, correcting the middle of a
///     timeline would silently swallow every period after it. If the
///     incoming value is identical to what is already current, the call is a
///     no-op: superseding to write the same value again would only add
///     noise to history.
///   - a real-world change: no row exists for this exact `valid_from`, so
///     this is a new period, arriving either after or before what is
///     already known. Its boundaries are computed, not assumed, so that
///     periods stay non-overlapping regardless of the order calls arrive in:
///       - if a still-open prior period contains this start
///         (`valid_from < new AND valid_to > new`), that period's end is now
///         known -- close it at `new`.
///       - the new row's own `valid_to` is capped at the earliest existing
///         `valid_from` strictly after it, if any, else `forever()`. Without
///         this cap, a period inserted BEFORE an existing later one would
///         stay open-ended and overlap it -- exactly the "two open rows, no
///         tiebreak" bug this cap exists to rule out by construction.
pub async fn set_attr(tx: &mut Tx<'_>, instrument_id: i64, attr: &str, value: &str,
                      valid_from: NaiveDate, source: &str, decision_id: Option<i64>)
    -> AppResult<()>
{
    let existing: Option<(String, NaiveDate)> = sqlx::query_as(
        "SELECT value, valid_to FROM instrument_attr
          WHERE instrument_id = $1 AND attr = $2 AND valid_from = $3
            AND system_to = 'infinity'")
        .bind(instrument_id).bind(attr).bind(valid_from)
        .fetch_optional(&mut **tx).await?;

    if let Some((existing_value, existing_valid_to)) = existing {
        // Correction: same exact period. A no-op if nothing actually changed.
        if existing_value == value {
            return Ok(());
        }
        sqlx::query(
            "UPDATE instrument_attr SET system_to = now()
              WHERE instrument_id = $1 AND attr = $2 AND valid_from = $3
                AND system_to = 'infinity'")
            .bind(instrument_id).bind(attr).bind(valid_from)
            .execute(&mut **tx).await?;
        sqlx::query(
            "INSERT INTO instrument_attr
               (instrument_id, attr, value, valid_from, valid_to, source, decision_id)
             VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(instrument_id).bind(attr).bind(value).bind(valid_from)
            .bind(existing_valid_to).bind(source).bind(decision_id)
            .execute(&mut **tx).await?;
        return Ok(());
    }

    // Real-world change: a new period, in either chronological direction.
    // $3 (valid_from) appears three times below -- as the new end date for a
    // still-open period it lands inside, and in that period's own bounds check.
    sqlx::query(
        "UPDATE instrument_attr SET valid_to = $3
          WHERE instrument_id = $1 AND attr = $2 AND system_to = 'infinity'
            AND valid_from < $3 AND valid_to > $3")
        .bind(instrument_id).bind(attr).bind(valid_from)
        .execute(&mut **tx).await?;

    // Cap the new period at whatever already-known period follows it, so an
    // insert into the middle (or before the earliest known period) cannot
    // overlap what comes after it.
    let next_start: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT min(valid_from) FROM instrument_attr
          WHERE instrument_id = $1 AND attr = $2 AND system_to = 'infinity'
            AND valid_from > $3")
        .bind(instrument_id).bind(attr).bind(valid_from)
        .fetch_one(&mut **tx).await?;
    let valid_to = next_start.unwrap_or_else(forever);

    sqlx::query(
        "INSERT INTO instrument_attr
           (instrument_id, attr, value, valid_from, valid_to, source, decision_id)
         VALUES ($1,$2,$3,$4,$5,$6,$7)")
        .bind(instrument_id).bind(attr).bind(value).bind(valid_from)
        .bind(valid_to).bind(source).bind(decision_id)
        .execute(&mut **tx).await?;
    Ok(())
}

/// The instrument's lifecycle ended on `at`. Every attribute still open past
/// that date is capped there -- the durable way to say "this stopped being
/// true," mirroring what `close_alias` already does for identifiers.
/// `valid_from < $2` is required rather than assumed: a caller passing an
/// `at` that does not postdate a period's start must not be allowed to
/// produce a row that fails `CHECK (valid_from < valid_to)`.
pub async fn close_attrs_at(tx: &mut Tx<'_>, instrument_id: i64, at: NaiveDate)
    -> AppResult<()>
{
    sqlx::query(
        "UPDATE instrument_attr SET valid_to = $2
          WHERE instrument_id = $1 AND system_to = 'infinity'
            AND valid_to > $2 AND valid_from < $2")
        .bind(instrument_id).bind(at).execute(&mut **tx).await?;
    Ok(())
}

pub async fn attrs(pool: &PgPool, instrument_id: i64, as_of: NaiveDate)
    -> AppResult<Vec<Attr>>
{
    Ok(sqlx::query_as::<_, Attr>(
        "SELECT id, instrument_id, attr, value, valid_from, valid_to, source
           FROM instrument_attr
          WHERE instrument_id = $1
            AND valid_from <= $2 AND valid_to > $2
            AND system_to = 'infinity'
          ORDER BY attr")
        .bind(instrument_id).bind(as_of).fetch_all(pool).await?)
}

/// Every attribute period we have ever believed for this instrument, not just
/// what is true as of some date -- mirrors `aliases` above, so a change reads
/// as two periods rather than only its current result (Task 16's detail panel).
pub async fn attrs_history(pool: &PgPool, instrument_id: i64) -> AppResult<Vec<Attr>> {
    Ok(sqlx::query_as::<_, Attr>(
        "SELECT id, instrument_id, attr, value, valid_from, valid_to, source
           FROM instrument_attr
          WHERE instrument_id = $1 AND system_to = 'infinity'
          ORDER BY attr, valid_from")
        .bind(instrument_id).fetch_all(pool).await?)
}

/// Propose a predecessor/successor relationship. Always a proposal: P0 7.2
/// established that Bloomberg exposes no successor field, so every link here is
/// inferred and a human must agree before anything follows it.
pub async fn propose_link(pool: &PgPool, predecessor_id: i64, successor_id: i64,
                          link_type: &str, effective_date: NaiveDate,
                          evidence: serde_json::Value) -> AppResult<i64>
{
    Ok(sqlx::query_scalar(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence)
         VALUES ($1,$2,$3,$4,$5) RETURNING id")
        .bind(predecessor_id).bind(successor_id).bind(link_type)
        .bind(effective_date).bind(evidence)
        .fetch_one(pool).await?)
}

/// A no-op against an already-confirmed link: without this guard a second
/// call would silently overwrite who confirmed it and when.
pub async fn confirm_link(pool: &PgPool, link_id: i64, by: &str) -> AppResult<()> {
    sqlx::query("UPDATE instrument_link SET confirmed_by = $2, confirmed_at = now()
                  WHERE id = $1 AND confirmed_by IS NULL")
        .bind(link_id).bind(by).execute(pool).await?;
    Ok(())
}

/// Only confirmed links are ever followed. A spinoff can legitimately produce
/// more than one confirmed successor, so every confirmed link is returned,
/// most recently effective first (`successor_id` breaks ties deterministically).
pub async fn confirmed_successors(pool: &PgPool, instrument_id: i64)
    -> AppResult<Vec<i64>>
{
    Ok(sqlx::query_scalar(
        "SELECT successor_id FROM instrument_link
          WHERE predecessor_id = $1 AND confirmed_by IS NOT NULL
          ORDER BY effective_date DESC, successor_id")
        .bind(instrument_id).fetch_all(pool).await?)
}
