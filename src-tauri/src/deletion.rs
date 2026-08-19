//! Removal of registry entities.
//!
//! Two shapes of removal, chosen per deletion rather than by a global setting:
//! Retire flips `active` (reversible, and already honoured by the fetch path in
//! `views.rs`), Purge deletes rows. Foreign keys stay restrictive on purpose --
//! see the design doc §3.3 -- so every purge spells out its DELETE order.
//!
//! `run` and `hit_ledger` are never touched. A run records work that really
//! happened and the ledger records budget that was really spent; both stay
//! truthful after the asset they mention is gone.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    AssetClass,
    Asset,
    Field,
    View,
    Schedule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    Retire,
    Purge,
}

/// What the confirm dialog shows. Every number comes from the database, so the
/// dialog never guesses at the blast radius.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeletionImpact {
    pub kind: EntityKind,
    pub id: i64,
    pub label: String,
    pub observations: i64,
    pub first_obs: Option<chrono::NaiveDate>,
    pub last_obs: Option<chrono::NaiveDate>,
    /// Views this entity belongs to (assets and fields only).
    pub views: i64,
    pub issues: i64,
    /// Runs recorded against this view (views only).
    pub runs: i64,
    /// Rows that reference this one and stand in the way (classes: assets +
    /// fields; views: schedules).
    pub children: i64,
    pub can_retire: bool,
    pub can_purge: bool,
    pub blocked_reason: Option<String>,
    /// True when `observations`/`first_obs`/`last_obs`/`issues` above
    /// describe history that a purge leaves on disk rather than deletes.
    /// Currently only `EntityKind::Asset` (a book entry): purging one removes
    /// the book entry and its view memberships, never the instrument's
    /// identity, its aliases, or its recorded observations -- see
    /// `purge_asset_tx`. Without this flag the dialog cannot be worded
    /// correctly for both cases: a field purge really does delete its
    /// observations, an asset purge never does.
    pub purge_keeps_history: bool,
}

async fn scalar(pool: &PgPool, sql: &str, id: i64) -> AppResult<i64> {
    let (n,): (i64,) = sqlx::query_as(sql).bind(id).fetch_one(pool).await?;
    Ok(n)
}

async fn label_of(pool: &PgPool, sql: &str, id: i64) -> AppResult<String> {
    let row: Option<(String,)> = sqlx::query_as(sql).bind(id).fetch_optional(pool).await?;
    row.map(|(l,)| l)
        .ok_or_else(|| AppError::Validation(format!("no such row: id {id}")))
}

pub async fn describe_deletion(
    pool: &PgPool,
    kind: EntityKind,
    id: i64,
) -> AppResult<DeletionImpact> {
    let mut impact = DeletionImpact {
        kind,
        id,
        label: String::new(),
        observations: 0,
        first_obs: None,
        last_obs: None,
        views: 0,
        issues: 0,
        runs: 0,
        children: 0,
        can_retire: false,
        can_purge: false,
        blocked_reason: None,
        purge_keeps_history: false,
    };

    match kind {
        // "Asset" here means a book entry: identity (instrument, aliases,
        // observations) lives elsewhere and a book-entry deletion never
        // touches it -- see purge_asset_tx. observations/issues below are
        // shown for context (how much history this instrument has), not as
        // a warning of data loss -- purge_keeps_history tells the frontend
        // to word it that way.
        EntityKind::Asset => {
            impact.label =
                label_of(pool, "SELECT label FROM book_entry WHERE instrument_id = $1", id)
                    .await?;
            let dates: (i64, Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) =
                sqlx::query_as(
                    "SELECT COUNT(*), MIN(obs_date), MAX(obs_date)
                     FROM observation WHERE instrument_id = $1")
                    .bind(id).fetch_one(pool).await?;
            impact.observations = dates.0;
            impact.first_obs = dates.1;
            impact.last_obs = dates.2;
            impact.views = scalar(
                pool, "SELECT COUNT(*) FROM view_instrument WHERE instrument_id = $1", id)
                .await?;
            impact.issues = scalar(
                pool, "SELECT COUNT(*) FROM ingest_issue WHERE instrument_id = $1", id)
                .await?;
            impact.can_retire = true;
            impact.can_purge = true;
            impact.purge_keeps_history = true;
        }
        EntityKind::Field => {
            impact.label = label_of(pool, "SELECT label FROM field_def WHERE id = $1", id).await?;
            let dates: (i64, Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) =
                sqlx::query_as(
                    "SELECT COUNT(*), MIN(obs_date), MAX(obs_date)
                     FROM observation WHERE field_id = $1")
                    .bind(id).fetch_one(pool).await?;
            impact.observations = dates.0;
            impact.first_obs = dates.1;
            impact.last_obs = dates.2;
            impact.views =
                scalar(pool, "SELECT COUNT(*) FROM view_field WHERE field_id = $1", id).await?;
            impact.issues =
                scalar(pool, "SELECT COUNT(*) FROM ingest_issue WHERE field_id = $1", id).await?;
            impact.can_retire = true;
            impact.can_purge = true;
        }
        EntityKind::View => {
            impact.label = label_of(pool, "SELECT name FROM view WHERE id = $1", id).await?;
            impact.runs = scalar(pool, "SELECT COUNT(*) FROM run WHERE view_id = $1", id).await?;
            impact.children =
                scalar(pool, "SELECT COUNT(*) FROM schedule WHERE view_id = $1", id).await?;
            impact.can_retire = true;
            impact.can_purge = impact.runs == 0;
            if !impact.can_purge {
                impact.blocked_reason = Some(format!(
                    "{} run(s) reference this view; retire it instead", impact.runs));
            }
        }
        EntityKind::AssetClass => {
            impact.label = label_of(pool, "SELECT name FROM asset_class WHERE id = $1", id).await?;
            let assets = scalar(
                pool, "SELECT COUNT(*) FROM book_entry WHERE asset_class_id = $1", id)
                .await?;
            let flds =
                scalar(pool, "SELECT COUNT(*) FROM field_def WHERE asset_class_id = $1", id).await?;
            impact.children = assets + flds;
            impact.can_retire = false; // no `active` column, and none is needed
            impact.can_purge = impact.children == 0;
            if !impact.can_purge {
                impact.blocked_reason = Some(format!(
                    "{assets} asset(s) and {flds} field(s) still belong to this class"));
            }
        }
        EntityKind::Schedule => {
            let vid = scalar(pool, "SELECT view_id FROM schedule WHERE id = $1", id).await?;
            impact.label =
                label_of(pool, "SELECT name FROM view WHERE id = $1", vid).await?;
            impact.can_retire = false; // a schedule already has its own `active` toggle
            impact.can_purge = true;
        }
    }
    Ok(impact)
}

/// A schedule holds no history: there is nothing to retire and nothing to
/// cascade. `drawn_for`/`drawn_at` live on the row and go with it.
pub async fn delete_schedule(pool: &PgPool, id: i64) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM schedule WHERE id = $1")
        .bind(id).execute(pool).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such schedule: id {id}")));
    }
    Ok(())
}

/// An asset class is a grouping, not data. It is deletable only when nothing
/// points at it -- there is no meaningful "retired class", because a retired
/// class would still have to answer for its assets.
pub async fn delete_asset_class(pool: &PgPool, id: i64) -> AppResult<()> {
    let assets =
        scalar(pool, "SELECT COUNT(*) FROM book_entry WHERE asset_class_id = $1", id).await?;
    let flds = scalar(pool, "SELECT COUNT(*) FROM field_def WHERE asset_class_id = $1", id).await?;
    if assets > 0 || flds > 0 {
        return Err(AppError::DeleteBlocked {
            reason: "asset class is not empty".into(),
            counts: vec![("asset(s)".into(), assets), ("field(s)".into(), flds)],
        });
    }
    let n = sqlx::query("DELETE FROM asset_class WHERE id = $1")
        .bind(id).execute(pool).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such asset class: id {id}")));
    }
    Ok(())
}

/// Purge a book entry -- the user's list, not the company's identity.
///
/// This deletes ONLY `view_instrument` (the membership rows) and `book_entry`
/// itself. It must never touch `instrument`, `instrument_alias`,
/// `instrument_attr` or `observation`: removing something from the book is
/// not the same claim as asserting the company never existed, and the
/// observation history recorded against `instrument_id` belongs to the
/// instrument, not to the book entry that happened to be watching it.
///
/// Kept named `purge_asset_tx` (not `purge_book_entry_tx`) because
/// `bulk/mod.rs` imports it by this name; that module still targets the
/// removed `asset` table and is retargeted by a later task, out of scope
/// here (see task 12 correction G) -- renaming this helper too would only
/// break that import for no benefit.
pub async fn purge_asset_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> AppResult<()> {
    sqlx::query("DELETE FROM view_instrument WHERE instrument_id = $1")
        .bind(id).execute(&mut **tx).await?;
    let n = sqlx::query("DELETE FROM book_entry WHERE instrument_id = $1")
        .bind(id).execute(&mut **tx).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such book entry: id {id}")));
    }
    Ok(())
}

pub async fn purge_field_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> AppResult<()> {
    sqlx::query("DELETE FROM ingest_issue WHERE field_id = $1")
        .bind(id).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM observation WHERE field_id = $1")
        .bind(id).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM view_field WHERE field_id = $1")
        .bind(id).execute(&mut **tx).await?;
    let n = sqlx::query("DELETE FROM field_def WHERE id = $1")
        .bind(id).execute(&mut **tx).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such field: id {id}")));
    }
    Ok(())
}

pub async fn delete_book_entry(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()> {
    match mode {
        DeleteMode::Retire => {
            let n = sqlx::query("UPDATE book_entry SET active = false WHERE instrument_id = $1")
                .bind(id).execute(pool).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such book entry: id {id}")));
            }
            Ok(())
        }
        DeleteMode::Purge => {
            let mut tx = pool.begin().await?;
            purge_asset_tx(&mut tx, id).await?;
            tx.commit().await?;
            Ok(())
        }
    }
}

pub async fn delete_field(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()> {
    match mode {
        DeleteMode::Retire => {
            let n = sqlx::query("UPDATE field_def SET active = false WHERE id = $1")
                .bind(id).execute(pool).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such field: id {id}")));
            }
            Ok(())
        }
        DeleteMode::Purge => {
            let mut tx = pool.begin().await?;
            purge_field_tx(&mut tx, id).await?;
            tx.commit().await?;
            Ok(())
        }
    }
}

/// A view with runs behind it cannot honestly be purged: `run.view_id` would
/// dangle, and runs are never rewritten (design §3.4). Retire is always
/// available, and -- once the scheduler filters on `view.active` -- retiring
/// genuinely stops collection.
///
/// `view_instrument` and `view_field` are the only cascading foreign keys in
/// the schema, so they go with the view without an explicit statement.
pub async fn delete_view(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()> {
    match mode {
        DeleteMode::Retire => {
            let n = sqlx::query("UPDATE view SET active = false WHERE id = $1")
                .bind(id).execute(pool).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such view: id {id}")));
            }
            Ok(())
        }
        DeleteMode::Purge => {
            let runs = scalar(pool, "SELECT COUNT(*) FROM run WHERE view_id = $1", id).await?;
            if runs > 0 {
                return Err(AppError::DeleteBlocked {
                    reason: "this view has been run; retire it instead of purging".into(),
                    counts: vec![("run(s)".into(), runs)],
                });
            }
            let mut tx = pool.begin().await?;
            sqlx::query("DELETE FROM schedule WHERE view_id = $1")
                .bind(id).execute(&mut *tx).await?;
            let n = sqlx::query("DELETE FROM view WHERE id = $1")
                .bind(id).execute(&mut *tx).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such view: id {id}")));
            }
            tx.commit().await?;
            Ok(())
        }
    }
}
