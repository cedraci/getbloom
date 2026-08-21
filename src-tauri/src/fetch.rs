//! Fetcher-neutral types and BLPAPI request planning (spec A2 §2.4, §4.1).
//!
//! This module owns the vocabulary the pipeline speaks in — `ObsCell`,
//! `CellProblem`, `FetchOutcome` — independently of *how* the data was
//! obtained, so `ingest` and the orchestrator are fetcher-agnostic.
//!
//! Everything here is pure: no I/O, no process spawning, no Bloomberg. The
//! sidecar's JSON contract is modelled as plain structs so both directions of
//! it are unit-testable against the recorded fixtures in
//! `tests/fixtures/blpapi/`.

use crate::error::{AppError, AppResult};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Securities per BLPAPI request. Bloomberg accepts large batches, but keeping
/// them bounded keeps one failure from taking down an entire view's fetch.
pub const MAX_SECURITIES_PER_REQUEST: usize = 100;

// ---------------------------------------------------------------- core types

#[derive(Debug, Clone, PartialEq)]
pub enum CellValue {
    Num(f64),
    Text(String),
}

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub field_id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
    pub value_kind: String,
}

#[derive(Debug, Clone)]
pub struct ObsCell {
    pub instrument_id: i64,
    pub field_id: i64,
    pub obs_date: NaiveDate,
    pub value: CellValue,
}

#[derive(Debug, Clone)]
pub struct CellProblem {
    pub instrument_id: Option<i64>,
    pub field_id: Option<i64>,
    pub obs_date: Option<NaiveDate>,
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct FetchOutcome {
    pub cells: Vec<ObsCell>,
    pub problems: Vec<CellProblem>,
}

// ------------------------------------------------------------ request inputs

#[derive(Debug, Clone)]
pub struct FetchAsset {
    pub instrument_id: i64,
    pub asset_class_id: i64,
    pub class_name: String,
    pub label: String,
    pub bdp_security: String,
}

#[derive(Debug, Clone)]
pub struct FetchField {
    pub field_id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
    pub value_kind: String,
}

#[derive(Debug, Clone)]
pub struct FetchRequest {
    pub run_id: i64,
    pub assets: Vec<FetchAsset>,
    pub fields: Vec<FetchField>,
    /// `start == end` for a daily EOD run (Amendment A1).
    pub start: NaiveDate,
    pub end: NaiveDate,
}

impl FetchRequest {
    pub fn is_single_day(&self) -> bool {
        self.start == self.end
    }
}

// -------------------------------------------------------- sidecar wire types

/// A BLPAPI field override (e.g. `CDR` for a calendar code), serialized in
/// the sidecar's own shape -- see `blp_fetch.py`'s `overrides` handling in
/// `validate_request_spec` and `build_request`.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Override {
    #[serde(rename = "fieldId")]
    pub field_id: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct RequestSpec {
    pub kind: &'static str,
    pub securities: Vec<String>,
    pub fields: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub obs_date: Option<String>,
    /// Empty for now -- `plan_requests` does not yet compute CDR calendar
    /// codes (spec Open Question 3, a deferred live probe). Skipped when
    /// empty so the wire payload is byte-compatible with the pre-override
    /// sidecar contract.
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub overrides: Vec<Override>,
}

#[derive(Debug, Serialize)]
pub struct SidecarPayload {
    pub run_id: i64,
    pub timeout_s: u32,
    pub requests: Vec<RequestSpec>,
    /// Remote Bloomberg Terminal host/port (P10 task 7). Absent when unset,
    /// so an old config with no override sends exactly the wire shape it
    /// always did -- the sidecar's `payload.get("host", DEFAULT_HOST)` /
    /// `.get("port", DEFAULT_PORT)` fall back to localhost:8194.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

#[derive(Debug, Deserialize)]
pub struct SidecarObservation {
    pub security: String,
    pub field: String,
    pub date: String,
    pub num: Option<f64>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarProblem {
    pub security: Option<String>,
    pub field: Option<String>,
    pub date: Option<String>,
    pub code: String,
    #[serde(default)]
    pub detail: String,
}

/// One security × one bulk (table-valued) field, rows verbatim from the
/// sidecar's `parse_bulk_message`. Column names are Bloomberg's own, spaces
/// and all; nothing here interprets them (P3's corp-action ingester does).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarBulkRows {
    pub security: String,
    pub field: String,
    pub rows: Vec<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct SidecarResponse {
    pub status: String,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub detail: String,
    #[serde(default)]
    pub observations: Vec<SidecarObservation>,
    #[serde(default)]
    pub problems: Vec<SidecarProblem>,
    #[serde(default)]
    pub bulk_rows: Vec<SidecarBulkRows>,
}

// ------------------------------------------------------------ planning

fn compact(d: NaiveDate) -> String {
    d.format("%Y%m%d").to_string()
}

/// Group a view into BLPAPI requests (spec A2 §4.1).
///
/// Numeric and date fields go to `HistoricalDataRequest`; text fields have no
/// history and go to `ReferenceDataRequest`, stamped with the run's `obs_date`
/// exactly as Amendment A1 specifies.
///
/// Text fields are **omitted from a multi-day backfill**: stamping one live
/// reference value across a 30-day range would fabricate a history that was
/// never observed. Daily runs fill them in. This also fixes the Excel path's
/// bug of pushing text mnemonics through BDH, where they always failed.
pub fn plan_requests(req: &FetchRequest) -> AppResult<Vec<RequestSpec>> {
    if req.assets.is_empty() {
        return Err(AppError::Validation("view has no active assets".into()));
    }
    if req.start > req.end {
        return Err(AppError::Validation("start date after end date".into()));
    }

    let single_day = req.is_single_day();
    let mut by_class: BTreeMap<i64, (String, Vec<&FetchAsset>)> = BTreeMap::new();
    for a in &req.assets {
        by_class
            .entry(a.asset_class_id)
            .or_insert_with(|| (a.class_name.clone(), Vec::new()))
            .1
            .push(a);
    }

    let mut out = Vec::new();
    for (class_id, (class_name, assets)) in &by_class {
        let class_fields: Vec<&FetchField> = req
            .fields
            .iter()
            .filter(|f| f.asset_class_id == *class_id)
            .collect();
        if class_fields.is_empty() {
            return Err(AppError::Validation(format!(
                "no fields configured for asset class '{class_name}'"
            )));
        }

        let hist: Vec<String> = class_fields
            .iter()
            .filter(|f| f.value_kind != "text")
            .map(|f| f.mnemonic.clone())
            .collect();
        let text: Vec<String> = class_fields
            .iter()
            .filter(|f| f.value_kind == "text")
            .map(|f| f.mnemonic.clone())
            .collect();
        let securities: Vec<String> =
            assets.iter().map(|a| a.bdp_security.clone()).collect();

        if !hist.is_empty() {
            for chunk in securities.chunks(MAX_SECURITIES_PER_REQUEST) {
                out.push(RequestSpec {
                    kind: "history",
                    securities: chunk.to_vec(),
                    fields: hist.clone(),
                    start: Some(compact(req.start)),
                    end: Some(compact(req.end)),
                    obs_date: None,
                    overrides: Vec::new(),
                });
            }
        }
        if !text.is_empty() && single_day {
            for chunk in securities.chunks(MAX_SECURITIES_PER_REQUEST) {
                out.push(RequestSpec {
                    kind: "reference",
                    securities: chunk.to_vec(),
                    fields: text.clone(),
                    start: None,
                    end: None,
                    obs_date: Some(req.start.to_string()),
                    overrides: Vec::new(),
                });
            }
        }
    }
    Ok(out)
}

/// Hits actually dispatched to Bloomberg, computed from the planned wire
/// requests -- NOT the pre-flight gate estimate. The two differ: text fields
/// are dropped from multi-day ranges, and the gate estimate also folds in the
/// corp-action leg, which charges itself at the wire seam.
pub fn dispatched_hits(specs: &[RequestSpec], start: NaiveDate, end: NaiveDate) -> i64 {
    specs.iter().map(|s| {
        let per_day = (s.securities.len() * s.fields.len()) as i64;
        let days = if s.kind == "history" { crate::budget::weekdays_between(start, end) } else { 1 };
        per_day * days
    }).sum()
}

// ------------------------------------------------------------ response mapping

struct Lookup<'a> {
    by_security: HashMap<&'a str, &'a FetchAsset>,
    // Nested rather than keyed on a (i64, &str) tuple so a mnemonic borrowed
    // from the response (a shorter lifetime) can be looked up directly.
    by_class: HashMap<i64, HashMap<&'a str, &'a FetchField>>,
}

impl<'a> Lookup<'a> {
    fn new(req: &'a FetchRequest) -> Self {
        let mut by_class: HashMap<i64, HashMap<&'a str, &'a FetchField>> = HashMap::new();
        for f in &req.fields {
            by_class
                .entry(f.asset_class_id)
                .or_default()
                .insert(f.mnemonic.as_str(), f);
        }
        Self {
            by_security: req
                .assets
                .iter()
                .map(|a| (a.bdp_security.as_str(), a))
                .collect(),
            by_class,
        }
    }

    fn asset_for(&self, security: &str) -> Option<&'a FetchAsset> {
        self.by_security.get(security).copied()
    }

    fn field_for(&self, asset: &FetchAsset, mnemonic: &str) -> Option<&'a FetchField> {
        self.by_class
            .get(&asset.asset_class_id)
            .and_then(|m| m.get(mnemonic))
            .copied()
    }
}

/// Coerce a sidecar value against the configured `value_kind`.
///
/// The sidecar deliberately knows nothing about `field_def`, so this is where
/// the type contract is enforced — exactly, because BLPAPI declares its types
/// rather than rendering everything to a cell string.
fn coerce(value_kind: &str, num: Option<f64>, text: Option<&str>)
    -> Result<CellValue, String> {
    match value_kind {
        "numeric" => num.map(CellValue::Num).ok_or_else(|| {
            format!("expected numeric, got text {:?}", text.unwrap_or(""))
        }),
        "date" => {
            let t = text.ok_or_else(|| "expected a date, got a number".to_string())?;
            NaiveDate::parse_from_str(t, "%Y-%m-%d")
                .map(|d| CellValue::Text(d.format("%Y-%m-%d").to_string()))
                .map_err(|_| format!("expected a date, got '{t}'"))
        }
        // Reference fields occasionally return a number where text is expected
        // (a rating, a count). That is information, not an error, so render it
        // rather than discarding the observation.
        _ => Ok(CellValue::Text(match (text, num) {
            (Some(t), _) => t.to_string(),
            (None, Some(n)) => n.to_string(),
            (None, None) => return Err("no value".into()),
        })),
    }
}

fn problem(
    asset: Option<&FetchAsset>,
    field: Option<&FetchField>,
    date: Option<NaiveDate>,
    code: &str,
    detail: String,
) -> CellProblem {
    CellProblem {
        instrument_id: asset.map(|a| a.instrument_id),
        field_id: field.map(|f| f.field_id),
        obs_date: date,
        code: code.to_string(),
        detail,
    }
}

/// Translate the sidecar's security/mnemonic-keyed response into database ids.
pub fn map_response(req: &FetchRequest, resp: &SidecarResponse) -> FetchOutcome {
    let lookup = Lookup::new(req);
    let mut out = FetchOutcome::default();

    for o in &resp.observations {
        let Some(asset) = lookup.asset_for(&o.security) else {
            out.problems.push(problem(
                None, None, None, "unknown_security",
                format!("response contained unrequested security '{}'", o.security),
            ));
            continue;
        };
        let Some(field) = lookup.field_for(asset, &o.field) else {
            out.problems.push(problem(
                Some(asset), None, None, "unknown_field",
                format!("response contained unrequested field '{}'", o.field),
            ));
            continue;
        };
        let Ok(date) = NaiveDate::parse_from_str(&o.date, "%Y-%m-%d") else {
            out.problems.push(problem(
                Some(asset), Some(field), None, "bad_date",
                format!("unparseable date '{}'", o.date),
            ));
            continue;
        };
        match coerce(&field.value_kind, o.num, o.text.as_deref()) {
            Ok(value) => out.cells.push(ObsCell {
                instrument_id: asset.instrument_id,
                field_id: field.field_id,
                obs_date: date,
                value,
            }),
            Err(detail) => out.problems.push(problem(
                Some(asset), Some(field), Some(date), "type_mismatch", detail,
            )),
        }
    }

    for p in &resp.problems {
        let asset = p.security.as_deref().and_then(|s| lookup.asset_for(s));
        let field = asset
            .zip(p.field.as_deref())
            .and_then(|(a, m)| lookup.field_for(a, m));
        let date = p
            .date
            .as_deref()
            .and_then(|d| NaiveDate::parse_from_str(d, "%Y-%m-%d").ok());
        out.problems.push(CellProblem {
            instrument_id: asset.map(|a| a.instrument_id),
            field_id: field.map(|f| f.field_id),
            obs_date: date,
            code: p.code.clone(),
            detail: p.detail.clone(),
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    /// Two classes: Equity (2 numeric + 1 text) and Index (numeric only).
    fn sample(start: NaiveDate, end: NaiveDate) -> FetchRequest {
        FetchRequest {
            run_id: 7,
            assets: vec![
                FetchAsset { instrument_id: 1, asset_class_id: 10, class_name: "Equity".into(),
                             label: "Apple".into(), bdp_security: "AAPL US Equity".into() },
                FetchAsset { instrument_id: 2, asset_class_id: 10, class_name: "Equity".into(),
                             label: "LVMH".into(),
                             bdp_security: "/isin/FR0000121014 Equity".into() },
                FetchAsset { instrument_id: 3, asset_class_id: 20, class_name: "Index".into(),
                             label: "EuroStoxx".into(), bdp_security: "SX5E Index".into() },
            ],
            fields: vec![
                FetchField { field_id: 100, asset_class_id: 10, mnemonic: "PX_LAST".into(),
                             value_kind: "numeric".into() },
                FetchField { field_id: 101, asset_class_id: 10, mnemonic: "PX_VOLUME".into(),
                             value_kind: "numeric".into() },
                FetchField { field_id: 102, asset_class_id: 10, mnemonic: "NAME".into(),
                             value_kind: "text".into() },
                FetchField { field_id: 200, asset_class_id: 20, mnemonic: "PX_LAST".into(),
                             value_kind: "numeric".into() },
            ],
            start,
            end,
        }
    }

    #[test]
    fn eod_splits_numeric_to_history_and_text_to_reference() {
        let day = d(2026, 8, 17);
        let plan = plan_requests(&sample(day, day)).unwrap();
        // Equity: 1 history + 1 reference. Index: 1 history, no reference.
        assert_eq!(plan.len(), 3);

        let hist: Vec<_> = plan.iter().filter(|r| r.kind == "history").collect();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].fields, vec!["PX_LAST", "PX_VOLUME"]);
        assert_eq!(hist[0].securities,
                   vec!["AAPL US Equity", "/isin/FR0000121014 Equity"]);
        // Single-day EOD: start == end == obs_date (Amendment A1).
        assert_eq!(hist[0].start.as_deref(), Some("20260817"));
        assert_eq!(hist[0].end.as_deref(), Some("20260817"));

        let refs: Vec<_> = plan.iter().filter(|r| r.kind == "reference").collect();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].fields, vec!["NAME"]);
        assert_eq!(refs[0].obs_date.as_deref(), Some("2026-08-17"));
        // The Index class has no text field, so it gets no reference request.
        assert_eq!(refs[0].securities.len(), 2);
    }

    #[test]
    fn backfill_omits_text_fields_entirely() {
        // The Excel path pushed text mnemonics through BDH, where they always
        // failed. Here they are simply not requested over a range.
        let plan = plan_requests(&sample(d(2026, 7, 1), d(2026, 7, 31))).unwrap();
        assert!(plan.iter().all(|r| r.kind == "history"));
        assert_eq!(plan.len(), 2);
        assert!(plan.iter().all(|r| !r.fields.contains(&"NAME".to_string())));
        assert_eq!(plan[0].start.as_deref(), Some("20260701"));
        assert_eq!(plan[0].end.as_deref(), Some("20260731"));
    }

    #[test]
    fn securities_are_batched() {
        let day = d(2026, 8, 17);
        let mut req = sample(day, day);
        req.assets = (0..250)
            .map(|i| FetchAsset {
                instrument_id: i, asset_class_id: 20, class_name: "Index".into(),
                label: format!("A{i}"), bdp_security: format!("S{i} Index"),
            })
            .collect();
        let plan = plan_requests(&req).unwrap();
        assert_eq!(plan.len(), 3); // 100 + 100 + 50
        assert_eq!(plan[0].securities.len(), MAX_SECURITIES_PER_REQUEST);
        assert_eq!(plan[2].securities.len(), 50);
    }

    #[test]
    fn planning_rejects_empty_and_reversed() {
        let day = d(2026, 8, 17);
        let mut empty = sample(day, day);
        empty.assets.clear();
        assert!(plan_requests(&empty).is_err());
        assert!(plan_requests(&sample(d(2026, 8, 18), d(2026, 8, 17))).is_err());

        let mut no_fields = sample(day, day);
        no_fields.fields.retain(|f| f.asset_class_id != 20);
        let err = plan_requests(&no_fields).unwrap_err().to_string();
        assert!(err.contains("Index"), "got: {err}");
    }

    #[test]
    fn payload_serializes_without_null_keys() {
        let day = d(2026, 8, 17);
        let plan = plan_requests(&sample(day, day)).unwrap();
        let payload = SidecarPayload { run_id: 7, timeout_s: 120, requests: plan,
                                        host: None, port: None };
        let json = serde_json::to_string(&payload).unwrap();
        // history carries start/end and no obs_date; reference the reverse.
        assert!(json.contains(r#""kind":"history""#));
        assert!(json.contains(r#""obs_date":"2026-08-17""#));
        assert!(!json.contains("null"), "no null keys expected: {json}");
    }

    /// P10 task 7: host/port ride the wire ONLY when the user set them --
    /// None must vanish entirely (the sidecar's own localhost:8194 default
    /// takes over), Some must arrive with the exact values given.
    #[test]
    fn sidecar_payload_carries_host_only_when_set() {
        let day = d(2026, 8, 17);
        let plan = plan_requests(&sample(day, day)).unwrap();

        let none_payload = SidecarPayload { run_id: 7, timeout_s: 120, requests: plan.clone(),
                                             host: None, port: None };
        let json = serde_json::to_value(&none_payload).unwrap();
        assert!(json.get("host").is_none(), "None host must be absent from the wire: {json}");
        assert!(json.get("port").is_none(), "None port must be absent from the wire: {json}");

        let some_payload = SidecarPayload { run_id: 7, timeout_s: 120, requests: plan,
                                             host: Some("10.0.0.5".into()), port: Some(9194) };
        let json = serde_json::to_value(&some_payload).unwrap();
        assert_eq!(json["host"], "10.0.0.5");
        assert_eq!(json["port"], 9194);
    }

    fn resp(json: &str) -> SidecarResponse {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn maps_securities_and_mnemonics_to_ids() {
        let day = d(2026, 8, 17);
        let req = sample(day, day);
        let r = resp(r#"{"status":"ok","observations":[
            {"security":"AAPL US Equity","field":"PX_LAST","date":"2026-08-17","num":305.59},
            {"security":"AAPL US Equity","field":"NAME","date":"2026-08-17","text":"APPLE INC"},
            {"security":"SX5E Index","field":"PX_LAST","date":"2026-08-17","num":6530.45}],
            "problems":[]}"#);
        let out = map_response(&req, &r);
        assert_eq!(out.problems.len(), 0);
        assert_eq!(out.cells.len(), 3);
        assert_eq!(out.cells[0].instrument_id, 1);
        assert_eq!(out.cells[0].field_id, 100);
        assert_eq!(out.cells[0].value, CellValue::Num(305.59));
        assert_eq!(out.cells[1].field_id, 102);
        assert_eq!(out.cells[1].value, CellValue::Text("APPLE INC".into()));
        // Same mnemonic, different class -> different field_id.
        assert_eq!(out.cells[2].instrument_id, 3);
        assert_eq!(out.cells[2].field_id, 200);
    }

    #[test]
    fn problems_carry_ids_through() {
        let day = d(2026, 8, 17);
        let req = sample(day, day);
        let r = resp(r#"{"status":"ok","observations":[],"problems":[
            {"security":"AAPL US Equity","field":"PX_LAST","date":"2026-08-17",
             "code":"no_data","detail":"no trading day returned"}]}"#);
        let out = map_response(&req, &r);
        assert_eq!(out.cells.len(), 0);
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].instrument_id, Some(1));
        assert_eq!(out.problems[0].field_id, Some(100));
        assert_eq!(out.problems[0].obs_date, Some(day));
        assert_eq!(out.problems[0].code, "no_data");
    }

    #[test]
    fn unrequested_security_is_flagged_not_ingested() {
        let day = d(2026, 8, 17);
        let req = sample(day, day);
        let r = resp(r#"{"status":"ok","observations":[
            {"security":"MSFT US Equity","field":"PX_LAST","date":"2026-08-17","num":1.0}],
            "problems":[]}"#);
        let out = map_response(&req, &r);
        assert_eq!(out.cells.len(), 0);
        assert_eq!(out.problems[0].code, "unknown_security");
    }

    #[test]
    fn type_mismatch_is_exact() {
        let day = d(2026, 8, 17);
        let req = sample(day, day);
        // PX_LAST is numeric but came back as text.
        let r = resp(r#"{"status":"ok","observations":[
            {"security":"AAPL US Equity","field":"PX_LAST","date":"2026-08-17","text":"N.A."}],
            "problems":[]}"#);
        let out = map_response(&req, &r);
        assert_eq!(out.cells.len(), 0);
        assert_eq!(out.problems[0].code, "type_mismatch");
        assert_eq!(out.problems[0].instrument_id, Some(1));
        assert_eq!(out.problems[0].field_id, Some(100));
    }

    /// The sidecar has emitted `bulk_rows` since Task 5 of P1; the Rust side
    /// dropped it on the floor because SidecarResponse had no field for it.
    /// P3's corporate-action ingestion reads it, so the wire must carry it.
    #[test]
    fn sidecar_bulk_rows_are_carried_not_dropped() {
        let r = resp(r#"{"status":"ok","observations":[],"problems":[],
            "bulk_rows":[{"security":"AAPL US Equity","field":"EQY_DVD_ADJUST_FACT",
              "rows":[{"Adjustment Date":"2020-08-31","Adjustment Factor":4.0,
                       "Adjustment Factor Operator Type":1.0,
                       "Adjustment Factor Flag":3.0}]}]}"#);
        assert_eq!(r.bulk_rows.len(), 1);
        assert_eq!(r.bulk_rows[0].field, "EQY_DVD_ADJUST_FACT");
        assert_eq!(r.bulk_rows[0].rows[0]["Adjustment Factor"], 4.0);

        // A response without the key (old fixture, EOD run) still parses.
        let legacy = resp(r#"{"status":"ok","observations":[],"problems":[]}"#);
        assert!(legacy.bulk_rows.is_empty());
    }

    #[test]
    fn dispatched_hits_counts_only_planned_requests() {
        // 2 assets, 1 numeric field, 1 text field, 3-weekday range
        // (2026-08-17 Mon .. 2026-08-19 Wed). plan_requests drops the text
        // field on multi-day ranges, so dispatched = 2 secs x 1 field x 3 days = 6,
        // while the naive estimate (estimate_backfill_hits) would say
        // 2 x 2 x 3 = 12.
        let req = FetchRequest {
            run_id: 1,
            assets: vec![
                FetchAsset { instrument_id: 1, asset_class_id: 10, class_name: "Equity".into(),
                             label: "A".into(), bdp_security: "A US Equity".into() },
                FetchAsset { instrument_id: 2, asset_class_id: 10, class_name: "Equity".into(),
                             label: "B".into(), bdp_security: "B US Equity".into() },
            ],
            fields: vec![
                FetchField { field_id: 100, asset_class_id: 10, mnemonic: "PX_LAST".into(),
                             value_kind: "numeric".into() },
                FetchField { field_id: 101, asset_class_id: 10, mnemonic: "NAME".into(),
                             value_kind: "text".into() },
            ],
            start: d(2026, 8, 17),
            end: d(2026, 8, 19),
        };
        let specs = plan_requests(&req).unwrap();
        assert_eq!(dispatched_hits(&specs, req.start, req.end), 6);
    }

    #[test]
    fn overrides_serialize_in_sidecar_shape_and_vanish_when_empty() {
        let day = d(2026, 8, 17);
        let mut spec = plan_requests(&sample(day, day)).unwrap().remove(0);
        assert!(!serde_json::to_string(&spec).unwrap().contains("overrides"));
        spec.overrides.push(Override { field_id: "CDR".into(), value: "US".into() });
        let js = serde_json::to_value(&spec).unwrap();
        assert_eq!(js["overrides"][0]["fieldId"], "CDR");
        assert_eq!(js["overrides"][0]["value"], "US");
    }

    #[test]
    fn text_field_accepts_a_number_by_rendering_it() {
        let day = d(2026, 8, 17);
        let req = sample(day, day);
        let r = resp(r#"{"status":"ok","observations":[
            {"security":"AAPL US Equity","field":"NAME","date":"2026-08-17","num":42.0}],
            "problems":[]}"#);
        let out = map_response(&req, &r);
        assert_eq!(out.cells.len(), 1);
        assert_eq!(out.cells[0].value, CellValue::Text("42".into()));
    }
}
