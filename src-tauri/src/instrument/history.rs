//! Turning HISTORICAL_IDS_TIME_RANGE into alias validity periods.
//!
//! The anchoring rule is the whole point. P0 §6.4: asked about META US Equity
//! WITHOUT HISTORICAL_STARTING_IDENTIFIER, Bloomberg answers about the Roundhill
//! Ball Metaverse ETF, which also once wore the ticker META. The answer is
//! well-formed, plausible, and about a different company. So the anchor is a
//! required argument here, the column is NOT NULL by CHECK constraint, and an
//! identifier that already belongs to someone else is never absorbed.

use crate::error::{AppError, AppResult};
use crate::instrument::store::{self, NewAlias};
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

/// Split from `ingest` so the mapping can be exercised without a fetcher.
pub async fn apply(pool: &PgPool, instrument_id: i64, anchor: &str, rows: &[HistIdRow])
    -> AppResult<HistoryOutcome>
{
    let mut aliases_added = 0usize;
    let mut links_proposed = Vec::new();

    for row in rows {
        // Has this exact event already been applied? Bloomberg's Action ID is
        // stable, which is what makes re-ingestion cheap and idempotent -- and
        // is the key P3 will use to spot an amended or withdrawn change.
        if let Some(action) = &row.action_id {
            let seen: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM instrument_alias
                  WHERE instrument_id = $1 AND bbg_action_id = $2
                    AND system_to = 'infinity'")
                .bind(instrument_id).bind(action).fetch_one(pool).await?;
            if seen > 0 {
                continue;
            }
        }

        // Does either end of this change already belong to somebody else?
        //
        // Ownership is checked across ALL validity periods, not as of the change
        // date: the question is "is this identifier another instrument's, ever",
        // and an alias whose period has not started yet still answers it.
        //
        // The New ID is checked first, and it is what catches the META/METV case.
        // Anchored, the row reads FB -> META and META is our own current ticker,
        // so it falls through and FB becomes our alias. Unanchored, the row reads
        // META -> METV, and METV belongs to the Roundhill ETF. That is Bloomberg
        // telling us this chain is not ours. Absorbing it would attach another
        // company's identity to this instrument, so it becomes a proposal.
        if let Some(other) = owner_of(pool, &row.new_id).await? {
            if other != instrument_id {
                links_proposed.push(store::propose_link(
                    pool, instrument_id, other, "rename", row.date,
                    evidence(anchor, row,
                        "the New ID already belongs to another instrument; this \
                         chain of events is not this instrument's")).await?);
                continue;
            }
        }
        // The symmetric case: the Old ID is someone else's, so the change runs
        // from them to us. Same refusal, opposite direction.
        if let Some(other) = owner_of(pool, &row.old_id).await? {
            if other != instrument_id {
                links_proposed.push(store::propose_link(
                    pool, other, instrument_id, "rename", row.date,
                    evidence(anchor, row,
                        "the Old ID already belongs to another instrument; an \
                         automatic merge would destroy one of the two histories")).await?);
            }
            // Either it is ours already, or it is a proposal. Neither adds an alias.
            continue;
        }

        let mut tx = pool.begin().await?;
        // The old identifier was true from the start of the window until the change.
        store::insert_alias(&mut tx, instrument_id, &NewAlias {
            id_type: "ticker".into(),
            value: row.old_id.clone(),
            exch_code: row.old_exch.clone(),
            // Bloomberg gives the date the change took effect, not when the old
            // identifier began. The instrument's own start is the honest floor.
            valid_from: earliest_known(pool, instrument_id, row.date).await?,
            valid_to: Some(row.date),
            source: "bloomberg_hist_ids".into(),
            bbg_action_id: row.action_id.clone(),
            anchoring_identifier: Some(anchor.to_string()),
        }).await?;
        tx.commit().await?;
        aliases_added += 1;
    }

    Ok(HistoryOutcome { aliases_added, links_proposed })
}

/// Which instrument, if any, has ever worn this ticker.
///
/// Deliberately not as-of a date: `find_by_alias` answers "who wore this on that
/// day", which is the right question when resolving user input and the wrong one
/// here. An identifier whose validity period has not started yet is still
/// somebody's, and treating it as free is exactly how two histories get merged.
async fn owner_of(pool: &PgPool, ticker: &str) -> AppResult<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT instrument_id FROM instrument_alias
          WHERE id_type = 'ticker' AND lower(value) = lower($1)
            AND system_to = 'infinity'
          ORDER BY valid_from LIMIT 1")
        .bind(ticker).fetch_optional(pool).await?)
}

fn evidence(anchor: &str, row: &HistIdRow, why: &str) -> serde_json::Value {
    serde_json::json!({
        "field": "HISTORICAL_IDS_TIME_RANGE",
        "anchoring_identifier": anchor,
        "row": row,
        "why": why,
    })
}

/// The earliest validity start we already know for this instrument, or the day
/// before the change if we know nothing. Never later than `change_date`, because
/// an alias whose period is empty violates instrument_alias_period.
async fn earliest_known(pool: &PgPool, instrument_id: i64, change_date: NaiveDate)
    -> AppResult<NaiveDate>
{
    let found: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT min(valid_from) FROM instrument_alias
          WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(instrument_id).fetch_one(pool).await?;
    Ok(found.filter(|f| *f < change_date)
        .unwrap_or_else(|| change_date.pred_opt().unwrap_or(change_date)))
}
