//! Turning HISTORICAL_IDS_TIME_RANGE into alias validity periods.
//!
//! The anchoring rule is the whole point. P0 §6.4: asked about META US Equity
//! WITHOUT HISTORICAL_STARTING_IDENTIFIER, Bloomberg answers about the Roundhill
//! Ball Metaverse ETF, which also once wore the ticker META. The answer is
//! well-formed, plausible, and about a different company. So the anchor is a
//! required argument here, the column is NOT NULL by CHECK constraint, and an
//! identifier that already belongs to someone else is never absorbed.
//!
//! HISTORICAL_IDS_TIME_RANGE speaks in bare tickers plus exchange codes
//! ("FB", "US"); every other identity write in this codebase (`bind_identity`)
//! speaks in full security strings ("META US Equity") and never writes a bare
//! `ticker` alias at all. A merge-refusal check that only looked at `ticker`
//! rows would therefore never find a real, engine-bound instrument's identity
//! -- it would only ever see aliases a test hand-seeded. So every historical
//! identifier is written, and looked up, in both spaces: the bare `ticker`
//! (for the identifier's own record) and a reconstructed `bdp_security` (so
//! `owner_of` can recognise identity written by `bind_identity`, and so a
//! user typing "FB US Equity" finds Facebook -- the actual point of storing
//! this history at all).

use crate::error::{AppError, AppResult};
use crate::instrument::store::{self, NewAlias, Tx};
use crate::master_fetch::{HistIdRow, MasterFetcher};
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Serialize)]
pub struct HistoryOutcome {
    pub aliases_added: usize,
    /// instrument_link ids, all unconfirmed.
    pub links_proposed: Vec<i64>,
}

pub async fn ingest<F: MasterFetcher>(pool: &PgPool, fetcher: &F, instrument_id: i64,
                                      anchor: &str, start: NaiveDate)
    -> AppResult<HistoryOutcome>
{
    if anchor.trim().is_empty() {
        return Err(AppError::Validation(
            "identifier history requires an anchoring identifier (P0 6.4)".into()));
    }
    let rows = fetcher.hist_ids(anchor, anchor, start).await?;
    apply(pool, instrument_id, anchor, &rows).await
}

/// Who owns each end of a rename, decided once so every branch below reads
/// the same way. `Ambiguous` means more than one instrument has ever worn
/// the identifier (e.g. two live listings legitimately sharing a bare ticker
/// across markets, per `store.rs`'s BMW comment) -- not enough to tell which
/// chain this row belongs to, so nothing is asserted either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ownership { Free, Ours, Other(i64), Ambiguous }

fn decide(owners: &[i64], instrument_id: i64) -> Ownership {
    match owners {
        [] => Ownership::Free,
        [only] if *only == instrument_id => Ownership::Ours,
        [only] => Ownership::Other(*only),
        _ => Ownership::Ambiguous,
    }
}

/// Split from `ingest` so the mapping can be exercised without a fetcher.
///
/// Rows are processed oldest-first regardless of the order Bloomberg
/// returned them in, because each old identifier's period is bounded by its
/// neighbours in the chain, not computed independently per row -- see
/// `chain_start` below.
pub async fn apply(pool: &PgPool, instrument_id: i64, anchor: &str, rows: &[HistIdRow])
    -> AppResult<HistoryOutcome>
{
    let mut aliases_added = 0usize;
    let mut links_proposed = Vec::new();

    let mut sorted: Vec<&HistIdRow> = rows.iter().collect();
    sorted.sort_by_key(|r| r.date);

    let Some(first) = sorted.first() else {
        return Ok(HistoryOutcome { aliases_added, links_proposed });
    };
    let mut boundary = chain_start(pool, instrument_id, first.date).await?;
    // The current bdp_security period is corrected inside the writing arms
    // below, in the same transaction as the write it has to make room for --
    // never once at the end off a "last change date". A proposal or a refused
    // row must never move our own current period, and correcting afterwards
    // left an overlap committed in between.

    for row in &sorted {
        let period_start = boundary;
        // The event happened at this date whether or not we could safely
        // record it as our own alias -- so the boundary always advances,
        // even on a skipped row, to keep the rest of the chain's periods
        // correct.
        boundary = row.date;

        if already_applied(pool, instrument_id, row, period_start).await? {
            continue;
        }

        let new_sec = reconstruct_security(pool, instrument_id, &row.new_id,
                                           row.new_exch.as_deref()).await?;
        let old_sec = reconstruct_security(pool, instrument_id, &row.old_id,
                                           row.old_exch.as_deref()).await?;
        let new_owners = owner_of(pool, &row.new_id, new_sec.as_deref()).await?;
        let old_owners = owner_of(pool, &row.old_id, old_sec.as_deref()).await?;
        let new_own = decide(&new_owners, instrument_id);
        let old_own = decide(&old_owners, instrument_id);

        use Ownership::*;
        match (new_own, old_own) {
            // More than one instrument has ever worn one of the two
            // identifiers -- cannot tell which chain this row belongs to, so
            // nothing is asserted. Safe (no wrong write), but silently
            // dropping it would leave a completeness gap only stderr could
            // see -- so it gets the same durable-record treatment as an
            // instrument_link proposal, via ingest_issue instead.
            (Ambiguous, _) | (_, Ambiguous) => {
                eprintln!("identifier history: ambiguous ownership for {} -> {} on {} \
                           (anchor {anchor}), recording an issue", row.old_id, row.new_id, row.date);
                if new_own == Ambiguous {
                    record_ambiguous_owner_issue(pool, instrument_id, anchor, row,
                                                 &row.new_id, new_owners.clone()).await?;
                }
                if old_own == Ambiguous {
                    record_ambiguous_owner_issue(pool, instrument_id, anchor, row,
                                                 &row.old_id, old_owners.clone()).await?;
                }
            }
            // Both ends already belong to two DIFFERENT other instruments.
            // Neither is us -- this row is evidence about them, not about
            // the instrument we were asked to ingest for.
            (Other(new_other), Other(old_other)) => {
                if new_other != old_other {
                    if let Some(id) = propose_link_if_new(pool, old_other, new_other, "rename",
                        row.date, evidence(anchor, row,
                            "both the Old ID and the New ID already belong to other \
                             instruments; this chain of events is not this instrument's"))
                        .await? { links_proposed.push(id); }
                }
                // Else degenerate: the same other instrument owns both ends,
                // which is not evidence of anything involving anyone else.
            }
            // The New ID already belongs to someone else. This is the
            // META/METV trap: unanchored, "META" reads as though it became
            // "METV", and METV is the Roundhill ETF's, not ours. Checked
            // first, deliberately -- see the module doc.
            (Other(other), _) => {
                if let Some(id) = propose_link_if_new(pool, instrument_id, other, "rename",
                    row.date, evidence(anchor, row,
                        "the New ID already belongs to another instrument; this \
                         chain of events is not this instrument's"))
                    .await? { links_proposed.push(id); }
            }
            // The symmetric case: the Old ID is someone else's, so the
            // change runs from them to us.
            (_, Other(other)) => {
                if let Some(id) = propose_link_if_new(pool, other, instrument_id, "rename",
                    row.date, evidence(anchor, row,
                        "the Old ID already belongs to another instrument; an \
                         automatic merge would destroy one of the two histories"))
                    .await? { links_proposed.push(id); }
            }
            // P0 §6.5: the New ID is owned by nobody at all, so nothing
            // independently confirms that this chain is ours. Anchoring is
            // necessary but not sufficient -- a wrong anchor returns a
            // well-formed chain belonging to someone else, and the row is
            // evidence, not a fact. In particular this is where the META->METV
            // row lands on a clean database: `META US Equity` is ours,
            // `METV` is nobody's, and the pre-fix code read that as "our own
            // identifier just ended" and CLOSED our live alias, leaving the
            // instrument with no security valid today and silently
            // unfetchable. No successor instrument exists to point an
            // instrument_link at, so the durable record is an ingest_issue.
            (Free, _) => {
                eprintln!("identifier history: New ID {} belongs to no instrument \
                           (anchor {anchor}); refusing to write {} -> {} on {}",
                          row.new_id, row.old_id, row.new_id, row.date);
                record_unconfirmed_new_id_issue(pool, instrument_id, anchor, row).await?;
            }
            // Both ends are provably ours: we already carry the Old ID,
            // open-ended (typically a manual entry made before its history
            // was known), and the New ID is ours too. Bloomberg says the old
            // one stopped on this date -- close it instead of discarding the
            // event.
            (Ours, Ours) => {
                let mut tx = pool.begin().await?;
                close_open_alias(&mut tx, instrument_id, "ticker", &row.old_id, row.date).await?;
                if let Some(sec) = &old_sec {
                    close_open_alias(&mut tx, instrument_id, "bdp_security", sec, row.date).await?;
                }
                // After the closes, never before: if the old security WAS the
                // current open one it has just been closed at row.date, so
                // this finds nothing and returns -- rather than re-inserting
                // a period the close would then have to empty.
                correct_current_security_period_tx(&mut tx, instrument_id, row.date, anchor)
                    .await?;
                tx.commit().await?;
            }
            // The ordinary case, and the only one that writes a new alias:
            // the New ID is provably ours and nobody has claimed the Old ID.
            // Write it in both spaces -- bare ticker and reconstructed
            // security -- with the same period, the same Action ID, the same
            // anchor.
            (Ours, Free) => {
                let mut tx = pool.begin().await?;
                // BEFORE the inserts, and in the same transaction. The
                // current bdp_security still claims [listing_date, forever),
                // which overlaps the period about to be written for the old
                // identifier; correcting it afterwards (as this module used
                // to) left the overlap committed in between, where
                // `find_by_alias` had two answers to choose from. The alias
                // non-overlap constraint now refuses that outright, which is
                // how the window was found.
                correct_current_security_period_tx(&mut tx, instrument_id, row.date, anchor)
                    .await?;
                store::insert_alias(&mut tx, instrument_id, &NewAlias {
                    id_type: "ticker".into(), value: row.old_id.clone(),
                    exch_code: row.old_exch.clone(),
                    valid_from: period_start, valid_to: Some(row.date),
                    source: "bloomberg_hist_ids".into(),
                    bbg_action_id: row.action_id.clone(),
                    anchoring_identifier: Some(anchor.to_string()),
                }).await?;
                if let Some(sec) = &old_sec {
                    store::insert_alias(&mut tx, instrument_id, &NewAlias {
                        id_type: "bdp_security".into(), value: sec.clone(),
                        exch_code: row.old_exch.clone(),
                        valid_from: period_start, valid_to: Some(row.date),
                        source: "bloomberg_hist_ids".into(),
                        bbg_action_id: row.action_id.clone(),
                        anchoring_identifier: Some(anchor.to_string()),
                    }).await?;
                }
                tx.commit().await?;
                aliases_added += 1;
            }
        }
    }

    Ok(HistoryOutcome { aliases_added, links_proposed })
}

/// Which instrument(s), if any, have ever worn this identifier -- searched in
/// both the bare-ticker space (what a historical row's Old/New ID is) and,
/// when a security string can be reconstructed, the `bdp_security` space
/// (what `bind_identity` actually writes). Every distinct owner is returned,
/// never just one: `find_by_alias` answers "who wore this on that day", which
/// is the right question for resolving user input and the wrong one here --
/// an identifier whose validity period has not started yet is still
/// somebody's, and two live listings can legitimately share a bare ticker
/// across markets, so collapsing that to a single answer is exactly how a
/// wrong merge or a wrongly-refused rename happens.
async fn owner_of(pool: &PgPool, ticker: &str, security: Option<&str>) -> AppResult<Vec<i64>> {
    let mut owners: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT instrument_id FROM instrument_alias
          WHERE id_type = 'ticker' AND lower(value) = lower($1)
            AND system_to = 'infinity'")
        .bind(ticker).fetch_all(pool).await?;
    if let Some(sec) = security {
        let more: Vec<i64> = sqlx::query_scalar(
            "SELECT DISTINCT instrument_id FROM instrument_alias
              WHERE id_type = 'bdp_security' AND lower(value) = lower($1)
                AND system_to = 'infinity'")
            .bind(sec).fetch_all(pool).await?;
        for o in more {
            if !owners.contains(&o) {
                owners.push(o);
            }
        }
    }
    Ok(owners)
}

/// The full Bloomberg security string for a (ticker, exchange) pair drawn
/// from a historical row. HISTORICAL_IDS_TIME_RANGE reports a bare ticker
/// and exchange, never a yellow key, so this borrows the instrument's own --
/// on the assumption that the asset class does not change across a rename,
/// true of every case P0 captured. `None` if there is no exchange to key on,
/// or the instrument has no current (open-ended) `bdp_security` to borrow a
/// yellow key from yet.
async fn reconstruct_security(pool: &PgPool, instrument_id: i64, id: &str, exch: Option<&str>)
    -> AppResult<Option<String>>
{
    let Some(exch) = exch else { return Ok(None) };
    let current: Option<String> = sqlx::query_scalar(
        "SELECT value FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND system_to = 'infinity' AND valid_to = $2")
        .bind(instrument_id).bind(store::forever()).fetch_optional(pool).await?;
    Ok(current.and_then(|v| v.split_whitespace().last()
        .map(|yk| format!("{id} {exch} {yk}"))))
}

fn evidence(anchor: &str, row: &HistIdRow, why: &str) -> serde_json::Value {
    serde_json::json!({
        "field": "HISTORICAL_IDS_TIME_RANGE",
        "anchoring_identifier": anchor,
        "row": row,
        "why": why,
    })
}

/// The first old identifier's period starts here: whatever alias `valid_from`
/// already predates the first change (ordinarily the LISTING_DATE
/// `bind_identity` derived), or the day before the change if nothing is
/// known -- never later, since `instrument_alias_period` rejects an empty
/// range. Every subsequent old identifier in the chain starts at the
/// previous row's own date instead (see the `boundary` variable in `apply`),
/// so a chain of renames produces contiguous, non-overlapping periods rather
/// than each row guessing independently.
async fn chain_start(pool: &PgPool, instrument_id: i64, first_change: NaiveDate)
    -> AppResult<NaiveDate>
{
    let found: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT min(valid_from) FROM instrument_alias
          WHERE instrument_id = $1 AND system_to = 'infinity' AND valid_from < $2")
        .bind(instrument_id).bind(first_change).fetch_one(pool).await?;
    Ok(found.unwrap_or_else(|| first_change.pred_opt().unwrap_or(first_change)))
}

/// Has this exact event already been applied? Bloomberg's Action ID is
/// stable and checked scoped to this instrument, which is what makes
/// re-ingestion cheap and idempotent -- and is the key P3 will use to spot
/// an amended or withdrawn change. Not every row carries one: absent, fall
/// back to the alias's own natural key, (id_type, value, valid_from) --
/// exactly what `instrument_alias_current` uniquely indexes -- so a
/// re-ingest of such a row is a clean no-op instead of dying on a unique
/// violation.
async fn already_applied(pool: &PgPool, instrument_id: i64, row: &HistIdRow,
                         period_start: NaiveDate) -> AppResult<bool>
{
    if let Some(action) = &row.action_id {
        let seen: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM instrument_alias
              WHERE instrument_id = $1 AND bbg_action_id = $2 AND system_to = 'infinity'")
            .bind(instrument_id).bind(action).fetch_one(pool).await?;
        return Ok(seen > 0);
    }
    let seen: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'ticker' AND lower(value) = lower($2)
            AND valid_from = $3 AND system_to = 'infinity'")
        .bind(instrument_id).bind(&row.old_id).bind(period_start).fetch_one(pool).await?;
    Ok(seen > 0)
}

/// We already carry this identifier, open-ended. Close it at `at` rather
/// than leaving it valid forever once Bloomberg tells us it changed. A
/// no-op if it is already closed, which is what makes a second application
/// of the same event idempotent.
async fn close_open_alias(tx: &mut Tx<'_>, instrument_id: i64, id_type: &str,
                          value: &str, at: NaiveDate) -> AppResult<()>
{
    let alias_id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = $2 AND lower(value) = lower($3)
            AND system_to = 'infinity' AND valid_to = $4")
        .bind(instrument_id).bind(id_type).bind(value).bind(store::forever())
        .fetch_optional(&mut **tx).await?;
    if let Some(id) = alias_id {
        store::close_alias(tx, id, at).await?;
    }
    Ok(())
}

/// A no-op against a proposal already on file for this exact (predecessor,
/// successor, link_type, effective_date) -- the same tuple
/// `instrument_link_unique` guards at the database level -- so re-ingesting
/// the same response does not pile up duplicate proposals in the review
/// queue forever.
async fn propose_link_if_new(pool: &PgPool, predecessor_id: i64, successor_id: i64,
                             link_type: &str, effective_date: NaiveDate,
                             evidence: serde_json::Value) -> AppResult<Option<i64>>
{
    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM instrument_link
          WHERE predecessor_id = $1 AND successor_id = $2
            AND link_type = $3 AND effective_date = $4")
        .bind(predecessor_id).bind(successor_id).bind(link_type).bind(effective_date)
        .fetch_optional(pool).await?;
    if existing.is_some() {
        return Ok(None);
    }
    Ok(Some(store::propose_link(pool, predecessor_id, successor_id, link_type,
                                effective_date, evidence).await?))
}

/// More than one instrument has ever worn `id` -- `owner_of` cannot tell
/// which chain this row belongs to. Nothing is written to `instrument_alias`
/// or `instrument_link`: an N-ary ambiguity does not fit `instrument_link`'s
/// predecessor/successor shape, and this module does not stretch it to fit.
/// But silently dropping the row would leave a completeness gap only stderr
/// could see, so it gets `ingest_issue` -- this module's other durable,
/// queryable record for "cannot decide automatically". `severity = 'warn'`:
/// nothing is wrong, something is merely unresolved.
///
/// Idempotent the same way the rest of this module is: `detail` fully
/// encodes the row's identity (the identifier, the anchor, the change date,
/// the Action ID if any, and the sorted owner list), so an exact match on
/// (instrument_id, code, detail) is the natural key for "already recorded",
/// the same way `propose_link_if_new` matches on its own tuple.
async fn record_ambiguous_owner_issue(pool: &PgPool, instrument_id: i64, anchor: &str,
                                      row: &HistIdRow, id: &str, mut owners: Vec<i64>)
    -> AppResult<()>
{
    owners.sort_unstable();
    let detail = format!(
        "identifier={id} anchor={anchor} change_date={} action_id={} owners={owners:?}",
        row.date, row.action_id.as_deref().unwrap_or("none"));

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'ambiguous_identifier_owner' AND detail = $2")
        .bind(instrument_id).bind(&detail).fetch_optional(pool).await?;
    if existing.is_some() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
         VALUES (NULL, $1, 'warn', 'ambiguous_identifier_owner', $2)")
        .bind(instrument_id).bind(&detail)
        .execute(pool).await?;
    Ok(())
}

/// Bloomberg's New ID is not an identifier any instrument in the master
/// holds, so nothing independently confirms the chain is ours. P0 §6.5: a
/// returned row is evidence, not a fact, and the field cannot discover a
/// rename it does not already know about. No successor instrument exists to
/// hang an `instrument_link` proposal on, so the durable, queryable record is
/// an `ingest_issue`, exactly as for an ambiguous owner.
///
/// Idempotent on the same principle as `record_ambiguous_owner_issue`:
/// `detail` fully encodes the row, so (instrument_id, code, detail) is the
/// natural key for "already recorded".
async fn record_unconfirmed_new_id_issue(pool: &PgPool, instrument_id: i64, anchor: &str,
                                         row: &HistIdRow) -> AppResult<()>
{
    let detail = format!(
        "old_id={} new_id={} anchor={anchor} change_date={} action_id={} \
         reason=new_id_is_not_an_identifier_this_instrument_holds",
        row.old_id, row.new_id, row.date,
        row.action_id.as_deref().unwrap_or("none"));

    let existing: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'unconfirmed_identifier_change' AND detail = $2")
        .bind(instrument_id).bind(&detail).fetch_optional(pool).await?;
    if existing.is_some() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
         VALUES (NULL, $1, 'warn', 'unconfirmed_identifier_change', $2)")
        .bind(instrument_id).bind(&detail)
        .execute(pool).await?;
    Ok(())
}

/// `bind_identity` wrote the current identifier's own `bdp_security` alias
/// with `valid_from` = LISTING_DATE, which is false once a rename is known:
/// META did not exist as "META US Equity" at listing if it was born "FB US
/// Equity" and renamed later. Correct it as each of this instrument's OWN
/// changes is applied -- never off a row that turned out to be a proposal, a
/// refusal or an ambiguous skip. This is a correction of our own earlier
/// belief, so `system_to` supersession is the right mechanism, not `valid_to`,
/// which would falsely claim we always knew the boundary.
///
/// Takes the caller's transaction rather than opening its own: the correction
/// and the write it makes room for must be one atomic step, or the old period
/// and the new one overlap in between -- which the `instrument_alias_no_overlap`
/// constraint now refuses outright.
async fn correct_current_security_period_tx(tx: &mut Tx<'_>, instrument_id: i64,
                                            change_date: NaiveDate, anchor: &str)
    -> AppResult<()>
{
    let current: Option<(i64, String, Option<String>, NaiveDate)> = sqlx::query_as(
        "SELECT id, value, exch_code, valid_from FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND system_to = 'infinity' AND valid_to = $2")
        .bind(instrument_id).bind(store::forever()).fetch_optional(&mut **tx).await?;
    let Some((id, value, exch_code, valid_from)) = current else { return Ok(()) };
    if valid_from >= change_date {
        return Ok(());
    }
    store::supersede_alias(tx, id).await?;
    store::insert_alias(tx, instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value, exch_code,
        valid_from: change_date, valid_to: None,
        source: "bloomberg_hist_ids".into(), bbg_action_id: None,
        anchoring_identifier: Some(anchor.to_string()),
    }).await?;
    Ok(())
}
