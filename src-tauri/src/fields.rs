use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FieldDef {
    pub id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
    pub label: String,
    pub value_kind: String,
    /// P0 §5's machine-readable marker; 'BulkFormat' means table-valued. How
    /// P3 will know a field returns a table rather than a number.
    pub bbg_ftype: Option<String>,
    pub bbg_datatype: Option<String>,
    pub entitlement_note: String,
    pub active: bool,
}

pub fn normalize_mnemonic(m: &str) -> String {
    m.trim().to_uppercase()
}

pub fn validate_value_kind(k: &str) -> AppResult<()> {
    match k {
        "numeric" | "text" | "date" => Ok(()),
        other => Err(AppError::Validation(format!("invalid value_kind '{other}'"))),
    }
}

/// The configurable field-mapping layer (spec §4.9). `bbg_ftype` records P0
/// §5's `BulkFormat` marker -- how P3 will know a field returns a table
/// rather than a number -- so the layer has to be writable before P3, not
/// after.
#[allow(clippy::too_many_arguments)]
pub async fn create_field(
    pool: &PgPool,
    asset_class_id: i64,
    mnemonic: &str,
    label: &str,
    value_kind: &str,
    bbg_ftype: Option<&str>,
    bbg_datatype: Option<&str>,
    entitlement_note: &str,
) -> AppResult<FieldDef> {
    validate_value_kind(value_kind)?;
    Ok(sqlx::query_as::<_, FieldDef>(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                bbg_ftype, bbg_datatype, entitlement_note)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING *",
    )
    .bind(asset_class_id)
    .bind(normalize_mnemonic(mnemonic))
    .bind(label)
    .bind(value_kind)
    .bind(bbg_ftype)
    .bind(bbg_datatype)
    .bind(entitlement_note)
    .fetch_one(pool)
    .await?)
}

pub async fn list_fields(pool: &PgPool) -> AppResult<Vec<FieldDef>> {
    Ok(sqlx::query_as::<_, FieldDef>(
        "SELECT * FROM field_def ORDER BY asset_class_id, mnemonic",
    )
    .fetch_all(pool)
    .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mnemonic_uppercases_and_trims() {
        assert_eq!(normalize_mnemonic(" px_last "), "PX_LAST");
    }

    #[test]
    fn invalid_value_kind_rejected() {
        assert!(validate_value_kind("numeric").is_ok());
        assert!(validate_value_kind("text").is_ok());
        assert!(validate_value_kind("date").is_ok());
        assert!(validate_value_kind("blob").is_err());
    }
}
