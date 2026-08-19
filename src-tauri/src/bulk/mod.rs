//! Bulk asset management through an Excel round trip.
//!
//! Three files with hard boundaries, because that boundary is what makes the
//! interesting logic testable without Postgres or Excel:
//!   sheet.rs  -- files only, never the database
//!   diff.rs   -- pure functions, neither files nor the database
//!   mod.rs    -- the only place that does both

pub mod diff;
pub mod sheet;

use crate::deletion::{purge_asset_tx, DeleteMode};
use crate::error::{AppError, AppResult};
use crate::registry::resolve_bdp_security;
use diff::{DbAsset, ImportPlan};
use serde::{Deserialize, Serialize};
use sheet::ExportRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub added: i64,
    pub edited: i64,
    pub retired: i64,
    pub reactivated: i64,
    pub membership_updated: i64,
    pub removed: i64,
}

/// Every asset, flattened to the names the sheet and the differ speak in.
/// Inactive assets are included: the sheet is the whole registry, and an
/// `active` of "no" is how the user sees a retired name.
pub async fn load_db_assets(pool: &PgPool) -> AppResult<Vec<DbAsset>> {
    let rows: Vec<(i64, String, String, String, Option<String>, Option<String>,
                   String, bool, String)> = sqlx::query_as(
        "SELECT a.id, a.label, c.name, a.id_kind, a.ticker, a.isin,
                a.yellow_key, a.active, a.bdp_security
         FROM asset a JOIN asset_class c ON c.id = a.asset_class_id
         ORDER BY a.label")
        .fetch_all(pool).await?;

    let memberships: Vec<(i64, String)> = sqlx::query_as(
        "SELECT va.asset_id, v.name FROM view_asset va JOIN view v ON v.id = va.view_id")
        .fetch_all(pool).await?;
    let mut by_asset: HashMap<i64, Vec<String>> = HashMap::new();
    for (aid, name) in memberships {
        by_asset.entry(aid).or_default().push(name);
    }

    Ok(rows.into_iter().map(|(id, label, class, id_kind, ticker, isin,
                              yellow_key, active, bdp_security)| DbAsset {
        id, label, class, id_kind,
        ticker: ticker.unwrap_or_default(),
        isin: isin.unwrap_or_default(),
        yellow_key, active, bdp_security,
        views: by_asset.remove(&id).unwrap_or_default(),
    }).collect())
}

async fn view_names(pool: &PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM view ORDER BY name")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

async fn class_names(pool: &PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM asset_class ORDER BY name")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

pub async fn export_assets_xlsx(pool: &PgPool, path: &Path) -> AppResult<()> {
    let assets = load_db_assets(pool).await?;
    let views = view_names(pool).await?;
    let classes = class_names(pool).await?;
    let rows: Vec<ExportRow> = assets.into_iter().map(|a| ExportRow {
        id: a.id, label: a.label, class: a.class, id_kind: a.id_kind,
        ticker: a.ticker, isin: a.isin, yellow_key: a.yellow_key,
        active: a.active, security: a.bdp_security, views: a.views,
    }).collect();
    sheet::write_assets_sheet(path, &rows, &views, &classes)
}

async fn plan_for(pool: &PgPool, path: &Path) -> AppResult<ImportPlan> {
    let data = sheet::read_assets_sheet(path)?;
    let hash = sheet::file_sha256(path)?;
    let db = load_db_assets(pool).await?;
    let views = view_names(pool).await?;
    let classes = class_names(pool).await?;
    Ok(diff::diff(&data, &db, &classes, &views, &hash))
}

pub async fn preview_import(pool: &PgPool, path: &Path) -> AppResult<ImportPlan> {
    plan_for(pool, path).await
}

/// Re-reads and re-diffs the file, then applies everything or nothing.
///
/// The hash check is the point of the two phases: a plan the user reviewed can
/// never be applied against a file that changed underneath it. The re-diff
/// matters just as much -- the database may have moved on even when the file
/// has not.
pub async fn apply_import(
    pool: &PgPool,
    path: &Path,
    file_hash: &str,
    removal_modes: &[(i64, DeleteMode)],
    confirmed_removal_count: Option<i64>,
) -> AppResult<ImportResult> {
    let actual = sheet::file_sha256(path)?;
    if actual != file_hash {
        return Err(AppError::ImportRejected {
            reason: "the file changed since it was previewed; preview it again".into(),
        });
    }
    let plan = plan_for(pool, path).await?;
    if !plan.invalid_rows.is_empty() {
        return Err(AppError::ImportRejected {
            reason: format!("{} invalid row(s); nothing was applied", plan.invalid_rows.len()),
        });
    }
    if plan.requires_typed_confirmation
        && confirmed_removal_count != Some(plan.removals.len() as i64)
    {
        return Err(AppError::ImportRejected {
            reason: format!(
                "this removes {} of {} active assets; confirm the count to proceed",
                plan.removals.len(), plan.active_asset_count),
        });
    }

    let modes: HashMap<i64, DeleteMode> = removal_modes.iter().copied().collect();
    let classes: HashMap<String, i64> = {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM asset_class")
            .fetch_all(pool).await?;
        rows.into_iter().map(|(id, n)| (n, id)).collect()
    };
    let views: HashMap<String, i64> = {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM view")
            .fetch_all(pool).await?;
        rows.into_iter().map(|(id, n)| (n, id)).collect()
    };
    let class_id = |name: &str| -> AppResult<i64> {
        classes.get(name).copied()
            .ok_or_else(|| AppError::ImportRejected { reason: format!("no class '{name}'") })
    };
    let view_id = |name: &str| -> AppResult<i64> {
        views.get(name).copied()
            .ok_or_else(|| AppError::ImportRejected { reason: format!("no view '{name}'") })
    };

    let mut res = ImportResult::default();
    let mut tx = pool.begin().await?;

    for a in &plan.adds {
        // The security is recomputed here rather than trusted from the plan:
        // this is the last line of defence for the doubled-yellow-key fault.
        let sec = resolve_bdp_security(
            &a.id_kind,
            (!a.ticker.is_empty()).then_some(a.ticker.as_str()),
            (!a.isin.is_empty()).then_some(a.isin.as_str()),
            &a.yellow_key)?;
        let (new_id,): (i64,) = sqlx::query_as(
            "INSERT INTO asset (asset_class_id, label, id_kind, ticker, isin,
                                yellow_key, bdp_security, active)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
            .bind(class_id(&a.class)?).bind(&a.label).bind(&a.id_kind)
            .bind((!a.ticker.is_empty()).then(|| a.ticker.clone()))
            .bind((!a.isin.is_empty()).then(|| a.isin.clone()))
            .bind(a.yellow_key.trim()).bind(&sec).bind(a.active)
            .fetch_one(&mut *tx).await?;
        for v in &a.views {
            sqlx::query("INSERT INTO view_asset (view_id, asset_id) VALUES ($1,$2)")
                .bind(view_id(v)?).bind(new_id).execute(&mut *tx).await?;
        }
        res.added += 1;
    }

    for e in &plan.edits {
        let sec = resolve_bdp_security(
            &e.id_kind,
            (!e.ticker.is_empty()).then_some(e.ticker.as_str()),
            (!e.isin.is_empty()).then_some(e.isin.as_str()),
            &e.yellow_key)?;
        sqlx::query(
            "UPDATE asset SET asset_class_id = $2, label = $3, id_kind = $4,
                              ticker = $5, isin = $6, yellow_key = $7, bdp_security = $8
             WHERE id = $1")
            .bind(e.id).bind(class_id(&e.class)?).bind(&e.label).bind(&e.id_kind)
            .bind((!e.ticker.is_empty()).then(|| e.ticker.clone()))
            .bind((!e.isin.is_empty()).then(|| e.isin.clone()))
            .bind(e.yellow_key.trim()).bind(&sec)
            .execute(&mut *tx).await?;
        res.edited += 1;
    }

    for m in &plan.membership_changes {
        for v in &m.added {
            sqlx::query(
                "INSERT INTO view_asset (view_id, asset_id) VALUES ($1,$2)
                 ON CONFLICT DO NOTHING")
                .bind(view_id(v)?).bind(m.id).execute(&mut *tx).await?;
        }
        for v in &m.removed {
            sqlx::query("DELETE FROM view_asset WHERE view_id = $1 AND asset_id = $2")
                .bind(view_id(v)?).bind(m.id).execute(&mut *tx).await?;
        }
        res.membership_updated += 1;
    }

    for r in &plan.retires {
        sqlx::query("UPDATE asset SET active = false WHERE id = $1")
            .bind(r.id).execute(&mut *tx).await?;
        res.retired += 1;
    }
    for r in &plan.reactivations {
        sqlx::query("UPDATE asset SET active = true WHERE id = $1")
            .bind(r.id).execute(&mut *tx).await?;
        res.reactivated += 1;
    }

    // Removals last, so a purge never pulls the rug from under an edit above.
    // Retire is the default for anything the user did not decide explicitly.
    for r in &plan.removals {
        match modes.get(&r.id).copied().unwrap_or(DeleteMode::Retire) {
            DeleteMode::Retire => {
                sqlx::query("UPDATE asset SET active = false WHERE id = $1")
                    .bind(r.id).execute(&mut *tx).await?;
            }
            DeleteMode::Purge => purge_asset_tx(&mut tx, r.id).await?,
        }
        res.removed += 1;
    }

    tx.commit().await?;

    // Re-export over the same path so the file on disk matches what was just
    // committed. Without this, a blank-id add row keeps its blank id on disk
    // after the database has assigned it a real one, so re-previewing the same
    // file would read the now-persisted asset as both an invalid duplicate
    // claim on its own security and a removal (its id never appears as
    // "present" in a sheet where its row still has no id). Every other kind of
    // change already leaves the file matching the database post-apply -- edits,
    // membership marks, retires and reactivations all come from data the file
    // already held -- so adds are the only reason this step is required, but
    // it is cheap and correct to do unconditionally.
    export_assets_xlsx(pool, path).await?;

    Ok(res)
}
