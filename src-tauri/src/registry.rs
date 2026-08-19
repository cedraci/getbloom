use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssetClass {
    pub id: i64,
    pub name: String,
    pub description: String,
}

// Asset (the row), NewAsset, create_asset, list_assets and set_asset_active are
// gone: Task 9 replaces them with `book::BookEntry` / `book::add` /
// `book::list` / `book::set_active`, backed by `instrument` + `book_entry`
// rather than the old `asset` table.
//
// `resolve_bdp_security` and `strip_trailing_key` stay, unlike the brief for
// this task says to delete them: `bulk/mod.rs` and `bulk/diff.rs` still call
// `resolve_bdp_security` for real (xlsx export/import), not just as a type
// reference, and retargeting that to the instrument/resolution path is
// Task 12's work, not this one's. Keeping them here is the minimum needed to
// keep `cargo check --lib` green. The doubled-yellow-key regression these
// exist to prevent is *also* covered independently in
// `resolution::normalize::build_security` (Task 2) -- see
// `ticker_carrying_its_own_yellow_key_is_not_doubled` in this file and
// `a_ticker_carrying_its_own_yellow_key_is_not_doubled` in normalize.rs.

/// Drop a yellow key the user already typed onto the identifier.
///
/// The obvious thing to paste into a "ticker" box is the whole Bloomberg
/// identifier, "AAPL US Equity" -- while the yellow-key box next to it already
/// says "Equity". Appending blindly produced "AAPL US Equity Equity", which the
/// Terminal rejects as INVALID_SECURITY. The run then fails correctly but
/// incomprehensibly: the ticker looks perfectly valid in the UI, because the
/// duplication only exists in the derived `bdp_security`.
///
/// No real ticker ends in a whitespace-separated yellow key, so stripping one
/// is unambiguous. Comparison is case-insensitive because the Terminal accepts
/// "equity" and "Equity" alike.
fn strip_trailing_key(identifier: &str, yellow_key: &str) -> String {
    match identifier.rsplit_once(char::is_whitespace) {
        Some((head, tail))
            if tail.eq_ignore_ascii_case(yellow_key) && !head.trim().is_empty() =>
        {
            head.trim_end().to_string()
        }
        _ => identifier.to_string(),
    }
}

pub fn resolve_bdp_security(
    id_kind: &str,
    ticker: Option<&str>,
    isin: Option<&str>,
    yellow_key: &str,
) -> AppResult<String> {
    let yk = yellow_key.trim();
    if yk.is_empty() {
        return Err(AppError::Validation("yellow_key is required".into()));
    }
    match id_kind {
        "ticker" => {
            let t = ticker.map(str::trim).filter(|t| !t.is_empty())
                .ok_or_else(|| AppError::Validation("ticker required for id_kind=ticker".into()))?;
            let t = strip_trailing_key(t, yk);
            // A ticker that IS the yellow key ("Equity") never had a security in
            // it; stripping cannot help, so refuse rather than build "Equity Equity".
            if t.is_empty() || t.eq_ignore_ascii_case(yk) {
                return Err(AppError::Validation(
                    "ticker is only a yellow key -- enter the security, e.g. 'AAPL US'".into()));
            }
            Ok(format!("{t} {yk}"))
        }
        "isin" => {
            let i = isin.map(str::trim).filter(|i| !i.is_empty())
                .ok_or_else(|| AppError::Validation("isin required for id_kind=isin".into()))?;
            // Accept a pasted "/isin/FR0000120271 Corp" as readily as a bare ISIN.
            let i = strip_trailing_key(i, yk);
            let i = i.strip_prefix("/isin/").unwrap_or(&i).trim();
            if i.is_empty() {
                return Err(AppError::Validation("isin is empty after normalisation".into()));
            }
            Ok(format!("/isin/{i} {yk}"))
        }
        other => Err(AppError::Validation(format!("unknown id_kind '{other}'"))),
    }
}

pub async fn create_asset_class(pool: &PgPool, name: &str, description: &str) -> AppResult<AssetClass> {
    Ok(sqlx::query_as::<_, AssetClass>(
        "INSERT INTO asset_class (name, description) VALUES ($1, $2) RETURNING *")
        .bind(name).bind(description).fetch_one(pool).await?)
}

pub async fn list_asset_classes(pool: &PgPool) -> AppResult<Vec<AssetClass>> {
    Ok(sqlx::query_as::<_, AssetClass>("SELECT * FROM asset_class ORDER BY name")
        .fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_kind_joins_ticker_and_yellow_key() {
        let s = resolve_bdp_security("ticker", Some("AAPL US"), None, "Equity").unwrap();
        assert_eq!(s, "AAPL US Equity");
    }

    #[test]
    fn isin_kind_builds_slash_isin_form() {
        let s = resolve_bdp_security("isin", None, Some("FR0000120271"), "Corp").unwrap();
        assert_eq!(s, "/isin/FR0000120271 Corp");
    }

    #[test]
    fn missing_identifier_is_validation_error() {
        assert!(resolve_bdp_security("ticker", None, None, "Equity").is_err());
        assert!(resolve_bdp_security("isin", None, None, "Corp").is_err());
        assert!(resolve_bdp_security("cusip", Some("X"), None, "Corp").is_err());
    }

    /// Regression: runs 1 and 2 on 2026-08-17 both asked Bloomberg for
    /// "AAPL US Equity Equity" and were rejected with BAD_SEC/INVALID_SECURITY.
    #[test]
    fn ticker_carrying_its_own_yellow_key_is_not_doubled() {
        for input in ["AAPL US Equity", "AAPL US equity", "AAPL US EQUITY", "  AAPL US Equity  "] {
            assert_eq!(
                resolve_bdp_security("ticker", Some(input), None, "Equity").unwrap(),
                "AAPL US Equity",
                "input {input:?}");
        }
        // A different key is not a duplicate and must survive untouched.
        assert_eq!(
            resolve_bdp_security("ticker", Some("AAPL US Equity"), None, "Corp").unwrap(),
            "AAPL US Equity Corp");
        // Nothing left once the key is stripped is a user error, not a silent pass.
        assert!(resolve_bdp_security("ticker", Some("Equity"), None, "Equity").is_err());
    }

    #[test]
    fn isin_accepts_an_already_qualified_paste() {
        for input in ["FR0000120271", "/isin/FR0000120271", "/isin/FR0000120271 Corp"] {
            assert_eq!(
                resolve_bdp_security("isin", None, Some(input), "Corp").unwrap(),
                "/isin/FR0000120271 Corp",
                "input {input:?}");
        }
    }

    #[test]
    fn inputs_are_trimmed() {
        let s = resolve_bdp_security("ticker", Some(" AAPL US "), None, " Equity ").unwrap();
        assert_eq!(s, "AAPL US Equity");
    }
}
