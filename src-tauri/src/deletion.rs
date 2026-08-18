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
    };

    match kind {
        EntityKind::Asset => {
            impact.label = label_of(pool, "SELECT label FROM asset WHERE id = $1", id).await?;
            let dates: (i64, Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) =
                sqlx::query_as(
                    "SELECT COUNT(*), MIN(obs_date), MAX(obs_date)
                     FROM observation WHERE asset_id = $1")
                    .bind(id).fetch_one(pool).await?;
            impact.observations = dates.0;
            impact.first_obs = dates.1;
            impact.last_obs = dates.2;
            impact.views =
                scalar(pool, "SELECT COUNT(*) FROM view_asset WHERE asset_id = $1", id).await?;
            impact.issues =
                scalar(pool, "SELECT COUNT(*) FROM ingest_issue WHERE asset_id = $1", id).await?;
            impact.can_retire = true;
            impact.can_purge = true;
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
            let assets =
                scalar(pool, "SELECT COUNT(*) FROM asset WHERE asset_class_id = $1", id).await?;
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
    let assets = scalar(pool, "SELECT COUNT(*) FROM asset WHERE asset_class_id = $1", id).await?;
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
