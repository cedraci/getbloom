use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use crate::error::AppResult;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssetClass {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub corp_actions_capable: bool,
    pub ma_capable: bool,
    pub adjustment_style: String,
    pub qc_stale_days_default: Option<i32>,
}

// Asset (the row), NewAsset, create_asset, list_assets and set_asset_active are
// gone: Task 9 replaced them with `book::BookEntry` / `book::add` /
// `book::list` / `book::set_active`, backed by `instrument` + `book_entry`
// rather than the old `asset` table.
//
// `resolve_bdp_security` and `strip_trailing_key` are gone too, as of Task 13:
// they were a near-line-for-line duplicate of
// `resolution::normalize::build_security` (kept in parallel only because
// `bulk/` still called this copy). Task 13 retargeted `bulk/` onto
// `resolution::normalize::build_security` / `detect_id_kind` and deleted the
// copy here -- see `a_ticker_carrying_its_own_yellow_key_is_not_doubled` in
// `resolution::normalize` for the doubled-yellow-key regression test that now
// has exactly one home.

pub async fn create_asset_class(pool: &PgPool, name: &str, description: &str) -> AppResult<AssetClass> {
    Ok(sqlx::query_as::<_, AssetClass>(
        "INSERT INTO asset_class (name, description) VALUES ($1, $2) RETURNING *")
        .bind(name).bind(description).fetch_one(pool).await?)
}

pub async fn list_asset_classes(pool: &PgPool) -> AppResult<Vec<AssetClass>> {
    Ok(sqlx::query_as::<_, AssetClass>("SELECT * FROM asset_class ORDER BY name")
        .fetch_all(pool).await?)
}

/// The CHECK constraints (style whitelist, stale >= 2) surface as AppError --
/// the UI relays them verbatim rather than pre-validating.
pub async fn update_asset_class_capabilities(
    pool: &PgPool, id: i64, corp_actions_capable: bool, ma_capable: bool,
    adjustment_style: &str, qc_stale_days_default: Option<i32>) -> AppResult<()>
{
    sqlx::query(
        "UPDATE asset_class
         SET corp_actions_capable = $2, ma_capable = $3,
             adjustment_style = $4, qc_stale_days_default = $5
         WHERE id = $1")
        .bind(id).bind(corp_actions_capable).bind(ma_capable)
        .bind(adjustment_style).bind(qc_stale_days_default)
        .execute(pool).await?;
    Ok(())
}
