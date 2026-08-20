//! P3: corporate-action parsing and storage (design:
//! docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md).
//!
//! `payload` is the authority; the typed columns are extractions. The factor
//! field's column names are P0-measured; the dividend field's are NOT -- its
//! extraction goes through a candidate-name map and a row it cannot read is
//! stored, flagged (`fully_parsed = false`) and reported, never dropped.

use crate::fetch::SidecarBulkRows;
use chrono::NaiveDate;
use serde::Serialize;

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
