//! P3: corporate-action parsing and storage (design:
//! docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md).
//!
//! `payload` is the authority; the typed columns are extractions. The factor
//! field's column names are P0-measured; the dividend field's are NOT -- its
//! extraction goes through a candidate-name map and a row it cannot read is
//! stored, flagged (`fully_parsed = false`) and reported, never dropped.

use crate::error::AppResult;
use crate::fetch::SidecarBulkRows;
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;

pub const FACTOR_FIELD: &str = "EQY_DVD_ADJUST_FACT";
pub const DVD_FIELD: &str = "DVD_HIST_ALL_WITH_AMT_STATUS";
/// P0 10.1: this filter makes the factor call a superset -- splits AND cash
/// dividends in one request.
pub const CORP_ACTIONS_FILTER_VALUE: &str = "NORMAL_CASH|ABNORMAL_CASH|CAPITAL_CHANGE";

#[derive(Debug, Clone, Serialize)]
pub struct ParsedAction {
    pub source_field: String,
    pub natural_key: String,
    pub event_date: Option<NaiveDate>,
    pub amount: Option<f64>,
    pub operator: Option<i16>,
    pub flag: Option<i16>,
    pub dvd_type: Option<String>,
    pub frequency: Option<String>,
    pub declared_date: Option<NaiveDate>,
    pub record_date: Option<NaiveDate>,
    pub pay_date: Option<NaiveDate>,
    pub amount_status: Option<String>,
    pub payload: serde_json::Value,
    pub fully_parsed: bool,
}

fn get_date(row: &serde_json::Map<String, serde_json::Value>, keys: &[&str])
    -> Option<NaiveDate> {
    keys.iter().find_map(|k| row.get(*k)?.as_str()?.parse().ok())
}
fn get_num(row: &serde_json::Map<String, serde_json::Value>, keys: &[&str])
    -> Option<f64> {
    keys.iter().find_map(|k| row.get(*k)?.as_f64())
}
fn get_text(row: &serde_json::Map<String, serde_json::Value>, keys: &[&str])
    -> Option<String> {
    keys.iter().find_map(|k| row.get(*k)?.as_str().map(str::to_string))
}

pub fn parse_table(t: &SidecarBulkRows) -> Vec<ParsedAction> {
    t.rows.iter().map(|row| {
        let payload = serde_json::Value::Object(row.clone());
        if t.field == FACTOR_FIELD {
            // Column names measured in P0 (headline_report.json).
            let event_date = get_date(row, &["Adjustment Date"]);
            let amount = get_num(row, &["Adjustment Factor"]);
            let operator = get_num(row, &["Adjustment Factor Operator Type"])
                .map(|v| v as i16);
            let flag = get_num(row, &["Adjustment Factor Flag"]).map(|v| v as i16);
            let fully = event_date.is_some() && amount.is_some()
                && operator.is_some() && flag.is_some();
            let natural_key = match (event_date, operator, flag) {
                (Some(d), Some(o), Some(f)) => format!("{d}|{o}|{f}"),
                _ => payload.to_string(),
            };
            ParsedAction {
                source_field: t.field.clone(), natural_key, event_date, amount,
                operator, flag, dvd_type: None, frequency: None,
                declared_date: None, record_date: None, pay_date: None,
                amount_status: None, payload, fully_parsed: fully,
            }
        } else {
            // No P0 capture pins these names; candidates cover Bloomberg's
            // documented spellings. First live run verifies (design §1).
            let event_date = get_date(row, &["Ex-Date", "Ex Date", "Ex-Dt"]);
            let dvd_type = get_text(row, &["Dividend Type", "Div Type"]);
            let amount = get_num(row, &["Dividend Amount", "Amount Per Share",
                                        "Gross Amount"]);
            let fully = event_date.is_some() && dvd_type.is_some() && amount.is_some();
            let natural_key = match (event_date, dvd_type.as_deref()) {
                (Some(d), Some(ty)) => format!("{d}|{ty}"),
                _ => payload.to_string(),
            };
            ParsedAction {
                source_field: t.field.clone(), natural_key, event_date, amount,
                operator: None, flag: None, dvd_type,
                frequency: get_text(row, &["Dividend Frequency", "Frequency"]),
                declared_date: get_date(row, &["Declared Date"]),
                record_date: get_date(row, &["Record Date"]),
                pay_date: get_date(row, &["Payable Date", "Pay Date", "Payment Date"]),
                amount_status: get_text(row, &["Amount Status",
                                               "Dividend Amount Status"]),
                payload, fully_parsed: fully,
            }
        }
    }).collect()
}

// ------------------------------------------------------------------ storage

#[derive(Debug, Default, Serialize)]
pub struct RefreshSummary {
    pub inserted: u64,
    pub amended: u64,
    pub withdrawn: u64,
    pub unchanged: u64,
    pub unparsed: u64,
}

/// Fetch both bulk fields for the instrument's current security and diff the
/// full snapshot against the current rows. Per source_field:
/// new key -> insert; changed payload -> close + insert (amendment); key
/// missing from a NON-EMPTY snapshot -> close + ingest_issue (withdrawal);
/// empty snapshot -> touch nothing (a failed field is not a cancellation).
pub async fn refresh<F: crate::master_fetch::MasterFetcher>(
    pool: &PgPool, fetcher: &F, instrument_id: i64, as_of: NaiveDate)
    -> AppResult<RefreshSummary>
{
    let security = crate::instrument::store::current_security(pool, instrument_id, as_of)
        .await?
        .ok_or_else(|| crate::error::AppError::Validation(format!(
            "instrument {instrument_id} has no security valid as of {as_of}")))?;
    let answered = fetcher.corp_actions(std::slice::from_ref(&security)).await?;

    let mut summary = RefreshSummary::default();
    let mut tx = pool.begin().await?;
    for table in &answered.parsed {
        let actions = parse_table(table);
        if actions.is_empty() {
            continue; // empty = failed/absent field, never a mass cancellation
        }
        let fresh_keys: std::collections::HashSet<&str> =
            actions.iter().map(|a| a.natural_key.as_str()).collect();

        let current: Vec<(i64, String, serde_json::Value)> = sqlx::query_as(
            "SELECT id, natural_key, payload FROM corp_action
              WHERE instrument_id = $1 AND source_field = $2
                AND system_to = 'infinity'")
            .bind(instrument_id).bind(&table.field)
            .fetch_all(&mut *tx).await?;
        let by_key: std::collections::HashMap<&str, (i64, &serde_json::Value)> =
            current.iter().map(|(id, k, p)| (k.as_str(), (*id, p))).collect();

        for a in &actions {
            if !a.fully_parsed {
                summary.unparsed += 1;
            }
            match by_key.get(a.natural_key.as_str()) {
                Some((_, existing)) if **existing == a.payload => {
                    summary.unchanged += 1;
                }
                Some((id, _)) => {
                    sqlx::query("UPDATE corp_action SET system_to = now() WHERE id = $1")
                        .bind(id).execute(&mut *tx).await?;
                    insert_action(&mut tx, instrument_id, a).await?;
                    summary.amended += 1;
                }
                None => {
                    insert_action(&mut tx, instrument_id, a).await?;
                    summary.inserted += 1;
                }
            }
        }
        for (id, key, _) in &current {
            if !fresh_keys.contains(key.as_str()) {
                sqlx::query("UPDATE corp_action SET system_to = now() WHERE id = $1")
                    .bind(id).execute(&mut *tx).await?;
                sqlx::query(
                    "INSERT INTO ingest_issue (instrument_id, severity, code, detail)
                     VALUES ($1,'warn','corp_action_withdrawn',$2)")
                    .bind(instrument_id)
                    .bind(format!("{}: {} vanished from the fresh snapshot",
                                  table.field, key))
                    .execute(&mut *tx).await?;
                summary.withdrawn += 1;
            }
        }
    }
    if summary.unparsed > 0 {
        sqlx::query(
            "INSERT INTO ingest_issue (instrument_id, severity, code, detail)
             VALUES ($1,'warn','corp_action_unparsed',$2)")
            .bind(instrument_id)
            .bind(format!("{} row(s) stored with a fallback key; column-name \
                           map needs the first live run's shapes", summary.unparsed))
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(summary)
}

async fn insert_action(tx: &mut crate::instrument::store::Tx<'_>,
                       instrument_id: i64, a: &ParsedAction) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO corp_action
           (instrument_id, source_field, natural_key, event_date, amount,
            operator, flag, dvd_type, frequency, declared_date, record_date,
            pay_date, amount_status, payload)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
        .bind(instrument_id).bind(&a.source_field).bind(&a.natural_key)
        .bind(a.event_date).bind(a.amount).bind(a.operator).bind(a.flag)
        .bind(&a.dvd_type).bind(&a.frequency).bind(a.declared_date)
        .bind(a.record_date).bind(a.pay_date).bind(&a.amount_status)
        .bind(&a.payload)
        .execute(&mut **tx).await?;
    Ok(())
}

/// Current rows for the instrument-detail panel, newest event first. A
/// fallback (canonical-JSON) natural key starts with '{', which is how the
/// UI knows to show the unparsed marker.
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ActionRow {
    pub id: i64,
    pub source_field: String,
    pub event_date: Option<NaiveDate>,
    pub amount: Option<f64>,
    pub operator: Option<i16>,
    pub flag: Option<i16>,
    pub dvd_type: Option<String>,
    pub amount_status: Option<String>,
    pub pay_date: Option<NaiveDate>,
    pub fully_parsed_key: bool,
}

pub async fn list_current(pool: &PgPool, instrument_id: i64)
    -> AppResult<Vec<ActionRow>> {
    Ok(sqlx::query_as::<_, ActionRow>(
        "SELECT id, source_field, event_date, amount, operator, flag,
                dvd_type, amount_status, pay_date,
                (natural_key NOT LIKE '{%') AS fully_parsed_key
           FROM corp_action
          WHERE instrument_id = $1 AND system_to = 'infinity'
          ORDER BY event_date DESC NULLS LAST, id DESC")
        .bind(instrument_id).fetch_all(pool).await?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::SidecarBulkRows;

    const HEADLINE: &str = include_str!(
        "../../docs/superpowers/specs/blpapi-facts/headline_report.json");

    /// The committed AAPL capture: five splits, real Bloomberg column names.
    fn aapl_factor_table() -> SidecarBulkRows {
        let all: serde_json::Value = serde_json::from_str(HEADLINE).unwrap();
        let rows = all["plain::AAPL US Equity"][0]["securityData"][0]
            ["fieldData"]["EQY_DVD_ADJUST_FACT"].clone();
        SidecarBulkRows {
            security: "AAPL US Equity".into(),
            field: FACTOR_FIELD.into(),
            rows: serde_json::from_value(rows).unwrap(),
        }
    }

    #[test]
    fn the_p0_factor_capture_parses_with_dates_operators_and_flags() {
        let acts = parse_table(&aapl_factor_table());
        assert_eq!(acts.len(), 5, "AAPL's five splits");
        let a2020 = acts.iter().find(|a| a.natural_key == "2020-08-31|1|3").unwrap();
        assert_eq!(a2020.event_date, Some("2020-08-31".parse().unwrap()));
        assert_eq!(a2020.amount, Some(4.0));
        assert_eq!(a2020.operator, Some(1));
        assert_eq!(a2020.flag, Some(3));
        assert!(a2020.fully_parsed);
        assert!(acts.iter().all(|a| a.source_field == FACTOR_FIELD));
    }

    /// Dividend rows have no P0 capture; the parser extracts what the
    /// candidate-name map recognises and NEVER drops a row -- an
    /// unrecognised shape keeps its payload and gets a canonical-JSON key.
    #[test]
    fn dividend_rows_parse_tolerantly_and_unknown_shapes_survive() {
        let t = SidecarBulkRows {
            security: "AAPL US Equity".into(),
            field: DVD_FIELD.into(),
            rows: serde_json::from_value(serde_json::json!([
                {"Declared Date": "2026-07-31", "Ex-Date": "2026-08-10",
                 "Record Date": "2026-08-11", "Payable Date": "2026-08-14",
                 "Dividend Amount": 0.26, "Dividend Frequency": "Quarter",
                 "Dividend Type": "Regular Cash", "Amount Status": "Confirmed"},
                {"Mystery Column": "??"}
            ])).unwrap(),
        };
        let acts = parse_table(&t);
        assert_eq!(acts.len(), 2, "no row is ever dropped");
        let ok = &acts[0];
        assert_eq!(ok.natural_key, "2026-08-10|Regular Cash");
        assert_eq!(ok.amount, Some(0.26));
        assert_eq!(ok.pay_date, Some("2026-08-14".parse().unwrap()));
        assert_eq!(ok.amount_status.as_deref(), Some("Confirmed"));
        assert!(ok.fully_parsed);
        let odd = &acts[1];
        assert!(!odd.fully_parsed, "unknown shape is flagged, not guessed at");
        assert_eq!(odd.payload["Mystery Column"], "??", "payload is the authority");
        assert!(odd.natural_key.contains("Mystery Column"),
                "fallback key is the canonical row JSON, so the row still diffs");
    }

    /// P0 4: META's factor table has a pre-IPO row (2010-10-31, factor 5).
    /// Nothing here may assume a chain starts at listing.
    #[test]
    fn the_meta_pre_ipo_factor_row_is_kept() {
        let all: serde_json::Value = serde_json::from_str(HEADLINE).unwrap();
        let rows = all["plain::META US Equity"][0]["securityData"][0]
            ["fieldData"]["EQY_DVD_ADJUST_FACT"].clone();
        let t = SidecarBulkRows { security: "META US Equity".into(),
                                  field: FACTOR_FIELD.into(),
                                  rows: serde_json::from_value(rows).unwrap() };
        let acts = parse_table(&t);
        assert_eq!(acts.len(), 1);
        assert_eq!(acts[0].event_date, Some("2010-10-31".parse().unwrap()));
    }
}
