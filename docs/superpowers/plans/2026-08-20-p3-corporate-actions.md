# P3 Corporate-Action Ingestion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fetch, version and store the factor chain (`EQY_DVD_ADJUST_FACT`) and dividend history (`DVD_HIST_ALL_WITH_AMT_STATUS`) per instrument, as an explicit costed user action — the data P4's adjustment engine will consume.

**Architecture:** One new `corp_action` table (snapshot-diff, system-time bitemporal, close-only updates); one new `MasterFetcher::corp_actions` method riding the sidecar's existing `bulk_reference` kind and the now-carried `bulk_rows`; one new `corp_actions` Rust module owning parsing + diffing; one Tauri command + an Instrument-detail section. The EOD pipeline learns to refuse bulk fields instead of stringifying them.

**Tech Stack:** Rust (sqlx/Postgres, Tauri), Svelte 5, Python BLPAPI sidecar (no sidecar changes — `bulk_reference` and `parse_bulk_message` already exist and are tested).

**Spec:** `docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md` (this plan implements it exactly). Facts: `2026-08-19-blpapi-field-facts.md` §4, §5, §10.1; capture `blpapi-facts/headline_report.json` (`plain::AAPL US Equity` → `EQY_DVD_ADJUST_FACT`, real column names).

## Global Constraints

- Everything in the pipeline-hardening plan's Global Constraints applies (no editing `0001_init.sql`; append-only stores; P0-confirmed mnemonics only; hits charged at the wire seam; no hard cap).
- Depends on pipeline-hardening Task 2 (`SidecarBulkRows` in `fetch.rs`) and Task 3 (migration `0002` exists, so this plan's migration is `0003`).
- `DVD_HIST_ALL_WITH_AMT_STATUS` column names are NOT P0-captured: the parser must be tolerant (payload verbatim always; typed extraction best-effort; unparsed rows counted and reported, never dropped silently).
- Overrides on the request: `CORPORATE_ACTIONS_FILTER = NORMAL_CASH|ABNORMAL_CASH|CAPITAL_CHANGE` (P0 §10.1 measured this returns splits + dividends in one call).

---

### Task 1: Migration 0003 — `corp_action`

**Files:**
- Create: `src-tauri/migrations/0003_corp_action.sql`
- Test: `src-tauri/tests/schema.rs` (append)

**Interfaces:**
- Produces: table `corp_action` — consumed by Tasks 2–5. Natural-key uniqueness only among current rows; the only legal UPDATE closes `system_to`.

- [ ] **Step 1: Write the failing test** (append to `tests/schema.rs`)

```rust
/// corp_action is snapshot-diffed: one current row per
/// (instrument, source_field, natural_key); amendments close-and-insert;
/// nothing may rewrite a payload in place.
#[tokio::test]
#[ignore = "requires postgres"]
async fn corp_action_is_append_only_with_one_current_row_per_key() {
    let pool = common::pool().await;
    let inst = getbloomdata_lib::instrument::store::create(&pool).await.unwrap();
    let ins = |payload: &'static str| {
        let pool = pool.clone();
        let iid = inst.instrument_id;
        async move {
            sqlx::query(
                "INSERT INTO corp_action
                   (instrument_id, source_field, natural_key, event_date, amount, payload)
                 VALUES ($1,'EQY_DVD_ADJUST_FACT','2020-08-31|1|3','2020-08-31',4.0,$2::jsonb)")
                .bind(iid).bind(payload).execute(&pool).await
        }
    };
    ins(r#"{"Adjustment Factor":4.0}"#).await.unwrap();
    let dup = ins(r#"{"Adjustment Factor":5.0}"#).await;
    assert!(dup.is_err(), "a second CURRENT row for the same key must be refused");

    let rewrite = sqlx::query(
        "UPDATE corp_action SET payload = '{}'::jsonb WHERE instrument_id = $1")
        .bind(inst.instrument_id).execute(&pool).await;
    assert!(rewrite.is_err(), "payload rewrite must be refused by the trigger");

    sqlx::query("UPDATE corp_action SET system_to = now() WHERE instrument_id = $1")
        .bind(inst.instrument_id).execute(&pool).await
        .expect("closing system_to is the one permitted update");
    ins(r#"{"Adjustment Factor":5.0}"#).await
        .expect("after closing, the corrected row inserts");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo test --test schema -- --ignored corp_action_is_append_only`
Expected: FAIL — `relation "corp_action" does not exist`.

- [ ] **Step 3: Write the migration**

`src-tauri/migrations/0003_corp_action.sql`:
```sql
-- P3: corporate actions, snapshot-diffed per refresh (design:
-- docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md).
-- Bloomberg reports the FULL history on every call; a refresh diffs it
-- against the current rows. `payload` is the verbatim Bloomberg row and the
-- authority; the typed columns are best-effort extractions for P4 and the
-- UI (DVD_HIST_ALL_WITH_AMT_STATUS column names have no P0 capture, so the
-- extraction may be partial until the first live run pins them).
CREATE TABLE corp_action (
  id            BIGSERIAL PRIMARY KEY,
  instrument_id BIGINT NOT NULL REFERENCES instrument(instrument_id),
  source_field  TEXT NOT NULL CHECK (source_field IN
                  ('EQY_DVD_ADJUST_FACT','DVD_HIST_ALL_WITH_AMT_STATUS')),
  natural_key   TEXT NOT NULL,
  event_date    DATE,               -- adjustment date / ex-date
  amount        DOUBLE PRECISION,   -- factor / dividend amount per share
  operator      SMALLINT,           -- 1=div 2=mult 3=add; OPPOSITE for volume (P0 10.1)
  flag          SMALLINT,           -- 1=prices only, 3=prices and volumes
  dvd_type      TEXT,
  frequency     TEXT,
  declared_date DATE,
  record_date   DATE,
  pay_date      DATE,
  amount_status TEXT,               -- estimated vs confirmed (the _WITH_AMT_STATUS point)
  payload       JSONB NOT NULL,
  system_from   TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to     TIMESTAMPTZ NOT NULL DEFAULT 'infinity'
);

CREATE UNIQUE INDEX corp_action_current
  ON corp_action (instrument_id, source_field, natural_key)
  WHERE system_to = 'infinity';
CREATE INDEX corp_action_by_instrument ON corp_action (instrument_id, event_date);

CREATE FUNCTION corp_action_append_only() RETURNS trigger AS $fn$
BEGIN
  IF NEW.instrument_id <> OLD.instrument_id
     OR NEW.source_field <> OLD.source_field
     OR NEW.natural_key <> OLD.natural_key
     OR NEW.payload IS DISTINCT FROM OLD.payload
     OR NEW.event_date IS DISTINCT FROM OLD.event_date
     OR NEW.amount IS DISTINCT FROM OLD.amount THEN
    RAISE EXCEPTION
      'corp_action rows are append-only; close system_to and insert';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER corp_action_append_only BEFORE UPDATE ON corp_action
  FOR EACH ROW EXECUTE FUNCTION corp_action_append_only();
```

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo test --test schema -- --ignored`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/migrations/0003_corp_action.sql src-tauri/tests/schema.rs
git commit -m "feat(db): corp_action table, snapshot-diffed and close-only (migration 0003)"
```

---

### Task 2: The `corp_actions` module — parsing and natural keys

**Files:**
- Create: `src-tauri/src/corp_actions.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod corp_actions;`)

**Interfaces:**
- Consumes: `fetch::SidecarBulkRows` (pipeline-hardening Task 2).
- Produces (used by Tasks 3–5):
```rust
pub const FACTOR_FIELD: &str = "EQY_DVD_ADJUST_FACT";
pub const DVD_FIELD: &str = "DVD_HIST_ALL_WITH_AMT_STATUS";
pub const CORP_ACTIONS_FILTER_VALUE: &str = "NORMAL_CASH|ABNORMAL_CASH|CAPITAL_CHANGE";
pub struct ParsedAction {
    pub source_field: String,
    pub natural_key: String,
    pub event_date: Option<chrono::NaiveDate>,
    pub amount: Option<f64>,
    pub operator: Option<i16>,
    pub flag: Option<i16>,
    pub dvd_type: Option<String>,
    pub frequency: Option<String>,
    pub declared_date: Option<chrono::NaiveDate>,
    pub record_date: Option<chrono::NaiveDate>,
    pub pay_date: Option<chrono::NaiveDate>,
    pub amount_status: Option<String>,
    pub payload: serde_json::Value,
    pub fully_parsed: bool,
}
pub fn parse_table(t: &crate::fetch::SidecarBulkRows) -> Vec<ParsedAction>;
```

- [ ] **Step 1: Write the failing tests** (in `corp_actions.rs` `mod tests` — replay the committed P0 capture, the same way `master_fetch.rs` replays `histids_report.json`)

```rust
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
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo test --lib corp_actions`
Expected: FAIL — module does not exist (after adding `pub mod corp_actions;` with an empty file, missing items).

- [ ] **Step 3: Implement**

`src-tauri/src/corp_actions.rs` (parsing half):
```rust
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
            // documented spellings. First live run verifies (design 1).
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
```
Register in `lib.rs` after `pub mod commands;`: `pub mod corp_actions;`.

- [ ] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo test --lib corp_actions`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/corp_actions.rs src-tauri/src/lib.rs
git commit -m "feat: corp-action parsing -- P0-pinned factor columns, tolerant dividend extraction"
```

---

### Task 3: `MasterFetcher::corp_actions` — the wire seam

**Files:**
- Modify: `src-tauri/src/master_fetch.rs` (trait method, live impl, mock, hit-cost constant + test)

**Interfaces:**
- Produces:
```rust
pub const CORP_ACTIONS_FIELDS: [&str; 2] = [corp_actions::FACTOR_FIELD_LIT, ...]; // see below
pub const CORP_ACTIONS_HIT_COST: i64 = 2;  // 1 security x 2 fields
// on the trait:
fn corp_actions(&self, security: &str)
    -> impl Future<Output = AppResult<Answered<Vec<crate::fetch::SidecarBulkRows>>>> + Send;
```
(Constants live here as `pub const CORP_ACTIONS_FIELDS: [&str; 2] = ["EQY_DVD_ADJUST_FACT", "DVD_HIST_ALL_WITH_AMT_STATUS"];` — keep `corp_actions.rs`'s `FACTOR_FIELD`/`DVD_FIELD` referring to the same literals; a unit test pins they agree.)
- `MockMasterFetcher` gains `pub corp_actions_raw: serde_json::Value` (a `bulk_rows`-shaped array) and records `"corp_actions:<security>"`.

- [ ] **Step 1: Write the failing tests** (append to `master_fetch.rs` `mod tests`)

```rust
    /// The refresh cost is a promise to the budget screen: 1 security x 2
    /// bulk fields, same per-security-field unit as every other estimate.
    #[test]
    fn corp_actions_cost_matches_the_field_count() {
        assert_eq!(CORP_ACTIONS_FIELDS.len(), 2);
        assert_eq!(CORP_ACTIONS_HIT_COST, CORP_ACTIONS_FIELDS.len() as i64);
        assert_eq!(CORP_ACTIONS_FIELDS[0], crate::corp_actions::FACTOR_FIELD);
        assert_eq!(CORP_ACTIONS_FIELDS[1], crate::corp_actions::DVD_FIELD);
    }

    #[tokio::test]
    async fn the_mock_replays_corp_action_tables_and_records_the_call() {
        let mock = MockMasterFetcher {
            corp_actions_raw: serde_json::json!([
                {"security": "AAPL US Equity", "field": "EQY_DVD_ADJUST_FACT",
                 "rows": [{"Adjustment Date": "2020-08-31", "Adjustment Factor": 4.0,
                           "Adjustment Factor Operator Type": 1.0,
                           "Adjustment Factor Flag": 3.0}]}]),
            ..Default::default()
        };
        let ans = mock.corp_actions("AAPL US Equity").await.unwrap();
        assert_eq!(ans.parsed.len(), 1);
        assert_eq!(ans.parsed[0].field, "EQY_DVD_ADJUST_FACT");
        assert_eq!(mock.call_count(), 1);
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo test --lib master_fetch`
Expected: FAIL — missing items.

- [ ] **Step 3: Implement**

Constants (next to `IDENTITY_FIELDS`):
```rust
/// P3's one refresh request: both bulk fields in a single bulk_reference
/// call, with the corporate-actions filter that makes the factor chain a
/// superset (splits + cash dividends -- P0 10.1).
pub const CORP_ACTIONS_FIELDS: [&str; 2] =
    ["EQY_DVD_ADJUST_FACT", "DVD_HIST_ALL_WITH_AMT_STATUS"];
pub const CORP_ACTIONS_FILTER: &str = "CORPORATE_ACTIONS_FILTER";
/// 1 security x 2 fields, the standing per-security-field unit.
pub const CORP_ACTIONS_HIT_COST: i64 = CORP_ACTIONS_FIELDS.len() as i64;
```
Trait method (on `MasterFetcher`):
```rust
    /// Both corporate-action bulk fields for one security, verbatim tables.
    fn corp_actions(&self, security: &str)
        -> impl std::future::Future<
            Output = AppResult<Answered<Vec<crate::fetch::SidecarBulkRows>>>> + Send;
```
Live impl (on `BlpapiMasterFetcher`) — note it reads the sidecar's TOP-LEVEL `bulk_rows`, not `raw_messages`:
```rust
    async fn corp_actions(&self, security: &str)
        -> AppResult<Answered<Vec<crate::fetch::SidecarBulkRows>>>
    {
        let resp = self.call(serde_json::json!({
            "kind": "bulk_reference",
            "securities": [security],
            "fields": CORP_ACTIONS_FIELDS,
            "overrides": [{"fieldId": CORP_ACTIONS_FILTER,
                           "value": crate::corp_actions::CORP_ACTIONS_FILTER_VALUE}],
        })).await?;
        self.charge("corp_actions", CORP_ACTIONS_HIT_COST).await;
        let raw = resp["bulk_rows"].clone();
        let parsed: Vec<crate::fetch::SidecarBulkRows> =
            serde_json::from_value(raw.clone()).unwrap_or_default();
        Ok(Answered { parsed, raw })
    }
```
(Check how `blp_driver::run_raw`'s return exposes the sidecar response: `identity` reads `resp["raw_messages"]`, so the same `resp` carries `["bulk_rows"]`. If `run_raw` strips non-`raw_messages` keys, extend it to pass the full response through — read `blp_driver.rs` first.)

Mock: add field + `Default` entry + impl:
```rust
    async fn corp_actions(&self, security: &str)
        -> AppResult<Answered<Vec<crate::fetch::SidecarBulkRows>>>
    {
        self.record(&format!("corp_actions:{security}"));
        let parsed = serde_json::from_value(self.corp_actions_raw.clone())
            .unwrap_or_default();
        Ok(Answered { parsed, raw: self.corp_actions_raw.clone() })
    }
```

- [ ] **Step 4: Run to verify everything passes**

Run: `cd src-tauri && cargo test`
Expected: PASS (all unit tests, including every existing MockMasterFetcher consumer — they get the new method via the impl, no call-site changes).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/master_fetch.rs
git commit -m "feat: MasterFetcher::corp_actions -- one bulk_reference call, charged 2 hits at the seam"
```

---

### Task 4: Snapshot-diff refresh

**Files:**
- Modify: `src-tauri/src/corp_actions.rs` (storage half)
- Test: `src-tauri/tests/corp_actions.rs` (new file, `mod common;` harness)

**Interfaces:**
- Consumes: `MasterFetcher::corp_actions`, `store::current_security`, `parse_table`.
- Produces:
```rust
#[derive(Debug, Serialize)]
pub struct RefreshSummary { pub inserted: u64, pub amended: u64, pub withdrawn: u64,
                            pub unchanged: u64, pub unparsed: u64 }
pub async fn refresh<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
    instrument_id: i64, as_of: chrono::NaiveDate) -> AppResult<RefreshSummary>;
pub async fn list_current(pool: &PgPool, instrument_id: i64) -> AppResult<Vec<ActionRow>>;
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ActionRow { pub id: i64, pub source_field: String,
    pub event_date: Option<chrono::NaiveDate>, pub amount: Option<f64>,
    pub operator: Option<i16>, pub flag: Option<i16>,
    pub dvd_type: Option<String>, pub amount_status: Option<String>,
    pub pay_date: Option<chrono::NaiveDate>, pub fully_parsed_key: bool }
```
(`fully_parsed_key` = `natural_key NOT LIKE '{%'` — a canonical-JSON fallback key starts with `{`.)

- [ ] **Step 1: Write the failing tests** (`src-tauri/tests/corp_actions.rs`)

```rust
mod common;

use common::uniq;
use getbloomdata_lib::corp_actions::{self, RefreshSummary};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::master_fetch::MockMasterFetcher;

fn d(s: &str) -> chrono::NaiveDate { s.parse().unwrap() }

async fn instrument_with_security(pool: &sqlx::PgPool, stem: &str) -> (i64, String) {
    let inst = store::create(pool).await.unwrap();
    let sec = format!("{} US Equity", uniq(stem));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: sec.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    (inst.instrument_id, sec)
}

fn mock_with(rows: serde_json::Value) -> MockMasterFetcher {
    MockMasterFetcher { corp_actions_raw: rows, ..Default::default() }
}

fn factor_rows(sec: &str, factor: f64) -> serde_json::Value {
    serde_json::json!([{"security": sec, "field": "EQY_DVD_ADJUST_FACT",
        "rows": [{"Adjustment Date": "2020-08-31", "Adjustment Factor": factor,
                  "Adjustment Factor Operator Type": 1.0,
                  "Adjustment Factor Flag": 3.0}]}])
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_refresh_inserts_and_a_second_identical_refresh_converges() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CAONE").await;
    let mock = mock_with(factor_rows(&sec, 4.0));
    let s1 = corp_actions::refresh(&pool, &mock, iid, d("2026-08-20")).await.unwrap();
    assert_eq!((s1.inserted, s1.unchanged), (1, 0));
    let s2 = corp_actions::refresh(&pool, &mock, iid, d("2026-08-20")).await.unwrap();
    assert_eq!((s2.inserted, s2.unchanged), (0, 1), "identical snapshot inserts nothing");
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn an_amended_amount_supersedes_and_keeps_the_old_belief() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CATWO").await;
    corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 4.0)), iid,
                          d("2026-08-20")).await.unwrap();
    let s = corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 5.0)), iid,
                                  d("2026-08-20")).await.unwrap();
    assert_eq!(s.amended, 1);
    let rows: Vec<(f64, bool)> = sqlx::query_as(
        "SELECT amount, system_to = 'infinity' FROM corp_action
          WHERE instrument_id = $1 ORDER BY id")
        .bind(iid).fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![(4.0, false), (5.0, true)],
               "the old belief is closed, never destroyed");
}

/// A key that vanishes from a NON-EMPTY fresh snapshot is a withdrawn
/// action: closed, and reported as an ingest_issue.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_vanished_action_is_closed_and_reported() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CATHREE").await;
    let two = serde_json::json!([{"security": sec, "field": "EQY_DVD_ADJUST_FACT",
        "rows": [
          {"Adjustment Date": "2020-08-31", "Adjustment Factor": 4.0,
           "Adjustment Factor Operator Type": 1.0, "Adjustment Factor Flag": 3.0},
          {"Adjustment Date": "2014-06-09", "Adjustment Factor": 7.0,
           "Adjustment Factor Operator Type": 1.0, "Adjustment Factor Flag": 3.0}]}]);
    corp_actions::refresh(&pool, &mock_with(two), iid, d("2026-08-20")).await.unwrap();
    let s = corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 4.0)), iid,
                                  d("2026-08-20")).await.unwrap();
    assert_eq!(s.withdrawn, 1);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action
          WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(open, 1);
    let issues: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'corp_action_withdrawn'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(issues, 1);
}

/// An EMPTY table for a source_field must close nothing: a whole-history
/// cancellation is not a real scenario, but a failed field in the response
/// producing zero rows is -- and it must not wipe the local history.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_empty_snapshot_for_a_field_closes_nothing() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CAFOUR").await;
    corp_actions::refresh(&pool, &mock_with(factor_rows(&sec, 4.0)), iid,
                          d("2026-08-20")).await.unwrap();
    let s = corp_actions::refresh(&pool, &mock_with(serde_json::json!([])), iid,
                                  d("2026-08-20")).await.unwrap();
    assert_eq!(s.withdrawn, 0);
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM corp_action
          WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(open, 1, "an absent table is a failed fetch, not a cancellation");
}

/// Rows the tolerant parser could not type still land (payload verbatim,
/// canonical-JSON key) and are counted for the summary + ingest_issue.
#[tokio::test]
#[ignore = "requires postgres"]
async fn unparsed_rows_are_stored_flagged_and_counted() {
    let pool = common::pool().await;
    let (iid, sec) = instrument_with_security(&pool, "CAFIVE").await;
    let odd = serde_json::json!([{"security": sec,
        "field": "DVD_HIST_ALL_WITH_AMT_STATUS",
        "rows": [{"Unexpected Shape": 1}]}]);
    let s = corp_actions::refresh(&pool, &mock_with(odd), iid, d("2026-08-20"))
        .await.unwrap();
    assert_eq!((s.inserted, s.unparsed), (1, 1));
    let issues: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE instrument_id = $1 AND code = 'corp_action_unparsed'")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(issues, 1);
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo test --test corp_actions -- --ignored`
Expected: FAIL — `refresh` does not exist.

- [ ] **Step 3: Implement** (storage half of `corp_actions.rs`)

```rust
#[derive(Debug, Default, Serialize)]
pub struct RefreshSummary {
    pub inserted: u64, pub amended: u64, pub withdrawn: u64,
    pub unchanged: u64, pub unparsed: u64,
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
    let answered = fetcher.corp_actions(&security).await?;

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
        let by_key: std::collections::HashMap<&str, (&i64, &serde_json::Value)> =
            current.iter().map(|(id, k, p)| (k.as_str(), (id, p))).collect();

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
                        .bind(**id).execute(&mut *tx).await?;
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

async fn insert_action(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
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

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct ActionRow {
    pub id: i64, pub source_field: String,
    pub event_date: Option<NaiveDate>, pub amount: Option<f64>,
    pub operator: Option<i16>, pub flag: Option<i16>,
    pub dvd_type: Option<String>, pub amount_status: Option<String>,
    pub pay_date: Option<NaiveDate>, pub fully_parsed_key: bool,
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
```
(Use the crate's actual transaction alias if one exists — `store::Tx` — matching `insert_alias`'s signature style.)

- [ ] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo test --test corp_actions -- --ignored && cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/corp_actions.rs src-tauri/tests/corp_actions.rs
git commit -m "feat: corp-action snapshot-diff refresh -- insert, amend, withdraw, never destroy"
```

---

### Task 5: Command, UI, and the bulk-field guard in the EOD pipeline

**Files:**
- Modify: `src-tauri/src/commands.rs` (two commands), `src-tauri/src/lib.rs` (register)
- Modify: `src-tauri/src/orchestrator.rs` (`load_view` skips `BulkFormat` fields)
- Modify: `src/lib/api.ts`, `src/lib/InstrumentDetail.svelte`
- Test: `src-tauri/tests/pipeline.rs` (bulk-field guard)

**Interfaces:**
- Consumes: `corp_actions::{refresh, list_current, RefreshSummary, ActionRow}`, `views::view_fields` (`FieldDef.bbg_ftype`).
- Produces: Tauri commands `refresh_corp_actions(instrument_id) -> RefreshSummary`, `list_corp_actions(instrument_id) -> Vec<ActionRow>`.

- [ ] **Step 1: Write the failing guard test** (append to `tests/pipeline.rs`)

```rust
/// A BulkFormat field configured on a view must be SKIPPED by the EOD
/// pipeline (with an ingest_issue naming it), not stringified -- the exact
/// failure the sidecar's parse_bulk_message docstring warns about.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_bulk_field_on_a_view_is_skipped_with_an_issue_not_stringified() {
    let pool = common::pool().await;
    let (iid, _fid, vid, _rid) = scaffold(&pool, "BULKG").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    let bulk_fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, bbg_ftype)
         VALUES ($1,'DVD_HIST_ALL_WITH_AMT_STATUS','Dividends','text','BulkFormat')
         RETURNING id").bind(class).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(vid).bind(bulk_fid).execute(&pool).await.unwrap();

    struct Silent;
    impl DataFetcher for Silent {
        async fn fetch(&self, req: &FetchRequest, _a: Option<&Path>)
            -> AppResult<FetchOutcome> {
            assert!(req.fields.iter().all(|f| f.mnemonic != "DVD_HIST_ALL_WITH_AMT_STATUS"),
                    "a bulk field must never reach the fetcher");
            Ok(FetchOutcome::default())
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let cfg = PipelineConfig { data_dir: dir.path().to_path_buf(),
        python_path: "python".into(), script_path: "scripts/blp_fetch.py".into(),
        request_timeout_s: 5, soft_limit: 100_000 };
    orchestrator::run_eod_with(&pool, &cfg, &Silent, vid, "manual",
                               d("2026-08-18"), true).await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM ingest_issue
          WHERE code = 'bulk_field_skipped' AND detail LIKE '%DVD_HIST_ALL%'")
        .fetch_one(&pool).await.unwrap();
    assert!(n >= 1, "the skip must be visible, not silent");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored a_bulk_field_on_a_view_is_skipped`
Expected: FAIL — the bulk mnemonic reaches the fetcher (assertion inside `Silent`).

- [ ] **Step 3: Implement the guard** (in `orchestrator::load_view`, where `fields_db` is mapped)

```rust
    let mut fields = Vec::with_capacity(fields_db.len());
    for f in &fields_db {
        if f.bbg_ftype.as_deref() == Some("BulkFormat") {
            // plan_requests would coerce a table into one meaningless string
            // (the sidecar docstring's exact warning). Skipped and said out
            // loud; the data has its own path: the corporate-actions refresh.
            sqlx::query(
                "INSERT INTO ingest_issue (run_id, field_id, severity, code, detail)
                 VALUES (NULL, $1, 'warn', 'bulk_field_skipped', $2)")
                .bind(f.id)
                .bind(format!("bulk field {} skipped by the run pipeline; use \
                               the corporate-actions refresh instead", f.mnemonic))
                .execute(pool).await?;
            continue;
        }
        fields.push(FetchField { field_id: f.id, asset_class_id: f.asset_class_id,
                                 mnemonic: f.mnemonic.clone(),
                                 value_kind: f.value_kind.clone() });
    }
```
(Check `views::view_fields`' `FieldDef` exposes `bbg_ftype`; it is a column on `field_def`, so add it to the struct/query if missing.)

- [ ] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored`
Expected: PASS.

- [ ] **Step 5: Add the commands** (in `commands.rs`)

```rust
// ---------------------------------------------------------------------------
// Corporate actions (P3)
// ---------------------------------------------------------------------------

/// Explicit, costed user action (2 hits, charged at the wire seam under
/// purpose 'corp_actions'), same pattern as identifier history: never
/// automatic, never scheduled in P3.
#[tauri::command]
pub async fn refresh_corp_actions(state: State<'_, AppState>, instrument_id: i64)
    -> Result<crate::corp_actions::RefreshSummary, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg, pool: &state.pool };
    let as_of = chrono::Local::now().date_naive();
    crate::corp_actions::refresh(&state.pool, &fetcher, instrument_id, as_of).await
}

#[tauri::command]
pub async fn list_corp_actions(state: State<'_, AppState>, instrument_id: i64)
    -> Result<Vec<crate::corp_actions::ActionRow>, AppError> {
    crate::corp_actions::list_current(&state.pool, instrument_id).await
}
```
Register both in `lib.rs`'s `generate_handler!` (after `commands::instrument_attrs`).

- [ ] **Step 6: Wire the UI**

`src/lib/api.ts` — add types + calls following the file's existing pattern:
```ts
export interface RefreshCorpActionsSummary {
  inserted: number; amended: number; withdrawn: number;
  unchanged: number; unparsed: number;
}
export interface CorpActionRow {
  id: number; source_field: string; event_date: string | null;
  amount: number | null; operator: number | null; flag: number | null;
  dvd_type: string | null; amount_status: string | null;
  pay_date: string | null; fully_parsed_key: boolean;
}
// in the api object:
  refreshCorpActions: (instrumentId: number) =>
    invoke<RefreshCorpActionsSummary>("refresh_corp_actions", { instrumentId }),
  listCorpActions: (instrumentId: number) =>
    invoke<CorpActionRow[]>("list_corp_actions", { instrumentId }),
```
`src/lib/InstrumentDetail.svelte` — add a "Corporate actions" section under the identifier-history section, following its structure exactly: a table of `listCorpActions` rows (columns: Field, Event date, Amount, Op/Flag, Type, Status, Pay date; a `⚠ unparsed` marker when `!fully_parsed_key`), a "Refresh from Bloomberg (2 hits)" button calling `refreshCorpActions` then reloading the list and showing the summary line (`inserted / amended / withdrawn / unchanged / unparsed`), loading + error states matching the panel's existing ones.

Run: `npx svelte-check --threshold error` — no new errors.

- [ ] **Step 7: Full suite + commit**

Run: `cd src-tauri && cargo test && cargo test -- --ignored` (skip `smoke_real_bloomberg_end_to_end` if the Terminal is down: `cargo test -- --ignored --skip smoke_real`)
Expected: PASS.

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src-tauri/src/orchestrator.rs src-tauri/tests/pipeline.rs src/lib/api.ts src/lib/InstrumentDetail.svelte
git commit -m "feat: corporate-actions refresh command + instrument-detail panel; EOD pipeline refuses bulk fields"
```

---

## Live-verification note (first Terminal session)

Two things only the Terminal can pin, both flagged in code comments and the design doc §1:
1. `DVD_HIST_ALL_WITH_AMT_STATUS` column names — run one refresh on AAPL, check `corp_action.payload` and the `corp_action_unparsed` count; correct the candidate-name map if rows fall back.
2. That the AAPL factor refresh returns ~97 rows (5 splits + 92 dividends) with the filter override, matching P0 §10.1.
