//! The user's book: which instruments they care about, and what they call them.
//!
//! Identity is NOT here -- it belongs to `instrument`. What is here is the
//! label, the active flag and the class, which is exactly the part of the old
//! `asset` table that was genuinely the user's rather than Bloomberg's.
//!
//! There is deliberately no unique constraint on a security string. One
//! instrument wears several over its life (FB US Equity, then META US Equity),
//! so uniqueness on the string was not merely unnecessary -- it was wrong.

use crate::error::AppResult;
use crate::instrument::store;
use crate::master_fetch::MasterFetcher;
use crate::resolution::engine::{self, Resolution, ResolveInput};
use crate::resolution::score::Hints;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookEntry {
    pub instrument_id: i64,
    pub asset_class_id: i64,
    pub label: String,
    pub active: bool,
    pub note: String,
    /// Derived from today's alias; None when the instrument has no security
    /// string valid today (a delisted instrument, for instance).
    pub security: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddToBook {
    pub raw: String,
    pub yellow_key: String,
    pub asset_class_id: i64,
    pub label: String,
    #[serde(default)]
    pub hints: Hints,
}

#[derive(Debug, Serialize)]
pub enum AddOutcome {
    Added(BookEntry),
    NeedsReview { review_id: i64 },
    NotFound,
}

pub async fn add<F: MasterFetcher>(pool: &PgPool, fetcher: &F, req: &AddToBook,
                                   by: &str) -> AppResult<AddOutcome>
{
    let input = ResolveInput {
        raw: req.raw.clone(),
        yellow_key: req.yellow_key.clone(),
        hints: req.hints.clone(),
        as_of: chrono::Local::now().date_naive(),
        decided_by: by.to_string(),
    };
    match engine::resolve(pool, fetcher, &input).await? {
        Resolution::Bound { instrument_id, .. } => {
            // Re-adding an instrument already in the book updates its label
            // rather than failing on the primary key.
            sqlx::query(
                "INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)
                 ON CONFLICT (instrument_id) DO UPDATE
                   SET label = EXCLUDED.label, active = TRUE")
                .bind(instrument_id).bind(req.asset_class_id).bind(&req.label)
                .execute(pool).await?;
            let entry = get(pool, instrument_id).await?
                .expect("just inserted");
            // Tell the candidate cache this security is now a real instrument, so
            // search shows it as known rather than merely "seen before".
            if let Some(sec) = &entry.security {
                crate::instrument::search::link_candidate(pool, sec, instrument_id)
                    .await?;
            }
            Ok(AddOutcome::Added(entry))
        }
        Resolution::NeedsReview { review_id, .. } => {
            Ok(AddOutcome::NeedsReview { review_id })
        }
        Resolution::NotFound { .. } => Ok(AddOutcome::NotFound),
    }
}

pub async fn get(pool: &PgPool, instrument_id: i64) -> AppResult<Option<BookEntry>> {
    Ok(list(pool).await?.into_iter().find(|b| b.instrument_id == instrument_id))
}

pub async fn list(pool: &PgPool) -> AppResult<Vec<BookEntry>> {
    let rows: Vec<(i64, i64, String, bool, String)> = sqlx::query_as(
        "SELECT instrument_id, asset_class_id, label, active, note
           FROM book_entry ORDER BY label")
        .fetch_all(pool).await?;
    let today = chrono::Local::now().date_naive();
    let mut out = Vec::with_capacity(rows.len());
    for (instrument_id, asset_class_id, label, active, note) in rows {
        let security = store::current_security(pool, instrument_id, today).await?;
        out.push(BookEntry { instrument_id, asset_class_id, label, active, note,
                             security });
    }
    Ok(out)
}

pub async fn set_active(pool: &PgPool, instrument_id: i64, active: bool)
    -> AppResult<()>
{
    sqlx::query("UPDATE book_entry SET active = $2 WHERE instrument_id = $1")
        .bind(instrument_id).bind(active).execute(pool).await?;
    Ok(())
}

pub async fn set_note(pool: &PgPool, instrument_id: i64, note: &str) -> AppResult<()> {
    sqlx::query("UPDATE book_entry SET note = $2 WHERE instrument_id = $1")
        .bind(instrument_id).bind(note).execute(pool).await?;
    Ok(())
}
