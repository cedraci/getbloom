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
    /// P7 quality gate, all opt-in and numeric-only (validate_qc):
    /// flag <= 0 values / day-over-day moves above this % / a value
    /// repeated this many consecutive observations.
    pub qc_nonpositive: bool,
    pub qc_outlier_pct: Option<f64>,
    pub qc_stale_days: Option<i32>,
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

/// QC thresholds describe a numeric series; on a text or date field they are
/// a configuration mistake and the mistake should be said at save time, not
/// silently ignored at run time.
pub fn validate_qc(value_kind: &str, qc_nonpositive: bool,
                   qc_outlier_pct: Option<f64>, qc_stale_days: Option<i32>)
    -> AppResult<()> {
    if value_kind != "numeric"
        && (qc_nonpositive || qc_outlier_pct.is_some() || qc_stale_days.is_some()) {
        return Err(AppError::Validation(
            "quality checks apply to numeric fields only".into()));
    }
    Ok(())
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
    qc_nonpositive: bool,
    qc_outlier_pct: Option<f64>,
    qc_stale_days: Option<i32>,
) -> AppResult<FieldDef> {
    validate_value_kind(value_kind)?;
    validate_qc(value_kind, qc_nonpositive, qc_outlier_pct, qc_stale_days)?;
    Ok(sqlx::query_as::<_, FieldDef>(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                bbg_ftype, bbg_datatype, entitlement_note,
                                qc_nonpositive, qc_outlier_pct, qc_stale_days)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *",
    )
    .bind(asset_class_id)
    .bind(normalize_mnemonic(mnemonic))
    .bind(label)
    .bind(value_kind)
    .bind(bbg_ftype)
    .bind(bbg_datatype)
    .bind(entitlement_note)
    .bind(qc_nonpositive)
    .bind(qc_outlier_pct)
    .bind(qc_stale_days)
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

    #[test]
    fn qc_thresholds_are_numeric_only() {
        assert!(validate_qc("numeric", true, Some(30.0), Some(5)).is_ok());
        assert!(validate_qc("numeric", false, None, None).is_ok());
        assert!(validate_qc("text", false, None, None).is_ok());
        assert!(validate_qc("text", true, None, None).is_err());
        assert!(validate_qc("date", false, Some(30.0), None).is_err());
        assert!(validate_qc("text", false, None, Some(5)).is_err());
    }
}
