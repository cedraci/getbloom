use crate::error::AppResult;
use crate::fields::FieldDef;
use crate::registry::Asset;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct View {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub active: bool,
}

pub async fn create_view(pool: &PgPool, name: &str, description: &str) -> AppResult<View> {
    Ok(sqlx::query_as::<_, View>(
        "INSERT INTO view (name, description) VALUES ($1,$2) RETURNING *",
    )
    .bind(name)
    .bind(description)
    .fetch_one(pool)
    .await?)
}

pub async fn list_views(pool: &PgPool) -> AppResult<Vec<View>> {
    Ok(sqlx::query_as::<_, View>("SELECT * FROM view ORDER BY name")
        .fetch_all(pool)
        .await?)
}

pub async fn set_view_assets(
    pool: &PgPool,
    view_id: i64,
    asset_ids: &[i64],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_asset WHERE view_id = $1")
        .bind(view_id)
        .execute(&mut *tx)
        .await?;
    for aid in asset_ids {
        sqlx::query("INSERT INTO view_asset (view_id, asset_id) VALUES ($1,$2)")
            .bind(view_id)
            .bind(aid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_view_fields(
    pool: &PgPool,
    view_id: i64,
    field_ids: &[i64],
) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_field WHERE view_id = $1")
        .bind(view_id)
        .execute(&mut *tx)
        .await?;
    for fid in field_ids {
        sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
            .bind(view_id)
            .bind(fid)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn view_assets(pool: &PgPool, view_id: i64) -> AppResult<Vec<Asset>> {
    Ok(sqlx::query_as::<_, Asset>(
        "SELECT a.* FROM asset a
         JOIN view_asset va ON va.asset_id = a.id
         WHERE va.view_id = $1 AND a.active ORDER BY a.label",
    )
    .bind(view_id)
    .fetch_all(pool)
    .await?)
}

pub async fn view_fields(pool: &PgPool, view_id: i64) -> AppResult<Vec<FieldDef>> {
    let explicit = sqlx::query_as::<_, FieldDef>(
        "SELECT f.* FROM field_def f
         JOIN view_field vf ON vf.field_id = f.id
         WHERE vf.view_id = $1 AND f.active ORDER BY f.asset_class_id, f.mnemonic",
    )
    .bind(view_id)
    .fetch_all(pool)
    .await?;
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    // Spec default: all active fields of the classes present in the view's assets.
    Ok(sqlx::query_as::<_, FieldDef>(
        "SELECT DISTINCT f.* FROM field_def f
         JOIN asset a ON a.asset_class_id = f.asset_class_id
         JOIN view_asset va ON va.asset_id = a.id
         WHERE va.view_id = $1 AND f.active AND a.active
         ORDER BY f.asset_class_id, f.mnemonic",
    )
    .bind(view_id)
    .fetch_all(pool)
    .await?)
}
