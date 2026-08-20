# P7: Quality Gate & Currency Dimension Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the pipeline notice wrong numbers (per-field quality checks, supersession alerts, unexplained-silence detection, weekly verification re-fetch) and make currency a recorded dimension of every numeric observation (stamped at ingest, exposed in reads, cross-currency stitching refused).

**Architecture:** A new pure `quality` module runs post-ingest against the stored series and writes `ingest_issue` rows with a new `'quality'` severity — data-driven per-field thresholds on `field_def`, no hardcoded semantics. Restatement detection = a `value_superseded` issue whenever ingest closes a row, plus a weekly scheduled re-fetch of the trailing 5 weekdays so restatements are actually re-read. Currency = a new nullable `observation.currency` column stamped from the instrument's bitemporal `currency` attribute at ingest, guarded by the append-only trigger, and checked at every stitch junction.

**Tech Stack:** Rust (sqlx/Postgres, Tauri 2), Svelte 5, SQL migrations (sqlx migrate).

**Spec:** This plan is Phase P1+P2 of the 2026-08-20 tool assessment (senior-AM review). Key decisions, restated here so the plan is self-contained:
- Quality checks are **per-field opt-in config** (`field_def.qc_*` columns), because check applicability depends on what the field *is* (a price is never negative; a yield legitimately is). No mnemonic heuristics.
- The completeness check is parameterless: an instrument that was **requested but produced neither cells nor problems** in a run is an unexplained silence (`quality_no_response`). Holidays are excused automatically because Bloomberg answers them with `no_data` problems.
- Quality findings make a run `partial`, exactly like ingest problems.
- Supersession stays a `'warn'` issue attached to the run (visible in the run's issue list); it does **not** flip run status — a correction is legitimate, it just must be seen.
- The verify re-fetch is a **scheduled backfill** (kind `'backfill'`, trigger `'scheduled'`) over the trailing 5 weekdays, fired on a per-schedule ISO weekday (`verify_dow`, default Friday=5, NULL=off). It replaces that day's EOD run. If it would cross `HardConfirm`, it is skipped for the week and the normal one-day run fires instead (a scheduler cannot click a confirm box).
- Currency is stored **verbatim** from Bloomberg's `CRNCY` (so LSE names carry `GBp`, deliberately — pence are recorded as pence; raw storage never converts). A changed currency supersedes the row and raises `currency_changed`.
- Stitching across a junction whose two instruments carry **different current currencies** stops with an explanatory `stopped` message (GBp vs GBP counts as different). Volume series are exempt (not currency-denominated).

## Global Constraints

- Never destroy data: observations/aliases/attrs/corp_actions are close-and-insert only; DB triggers enforce it. Any new column on `observation` must be added to the `observation_append_only` trigger.
- `run` and `hit_ledger` rows are never rewritten or deleted.
- Hit charging happens at the wire seam (`master_fetch.rs`); no new Bloomberg calls are introduced by this plan (the verify run reuses the existing fetch path and existing estimate gates).
- There is deliberately **no hard budget cap** (standing user decision, 2026-08-20 pipeline hardening); the verify run gates at `HardConfirm` only.
- Migration files must be committed with LF line endings (`.gitattributes` pins `src-tauri/migrations` — commit 8ad7f29 is the cautionary tale; verify with `git ls-files --eol src-tauri/migrations` before committing a new migration).
- DB integration tests: `#[ignore = "requires postgres"]`, shared `bloom_test` DB via `tests/common/mod.rs::pool()`, every UNIQUE-constrained fixture value goes through `common::uniq()`.
- Tests run with: `cargo test` (pure) and `cargo test -- --ignored` (needs local Postgres with `BLOOM_TEST_DATABASE_URL` or default `postgres://postgres:postgres@localhost/bloom_test`). Frontend: `npm run check` (svelte-check) — there is no JS test runner.
- All `cargo` / `npm` commands run from `src-tauri/` / repo root respectively.
- Every commit message ends with the trailer: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- New Rust code follows house style: `//!` module docs explaining WHY, comments only for non-obvious constraints, `AppResult`/`AppError` error handling, advisory sub-steps log-and-continue (`eprintln!`) rather than failing a run that already ingested.

## File Structure

- Create: `src-tauri/migrations/0007_quality_and_verify.sql` — severity `'quality'`, `field_def.qc_*`, `schedule.verify_dow`/`last_verified_on`.
- Create: `src-tauri/migrations/0008_observation_currency.sql` — `observation.currency` + backfill + trigger extension.
- Create: `src-tauri/src/quality.rs` — pure checks + DB gate runner (single module: the pure half is `#[cfg(test)]`-tested inline, the DB half mirrors `corp_actions.rs`'s split).
- Create: `src-tauri/tests/quality.rs` — schema assertions + gate integration tests.
- Create: `src-tauri/tests/currency.rs` — currency stamping, `currency_changed`, stitch guard tests.
- Modify: `src-tauri/src/fields.rs`, `ingest.rs`, `orchestrator.rs`, `scheduler.rs`, `commands.rs`, `dataview.rs`, `stitch.rs`, `lib.rs`.
- Modify: `src/lib/api.ts`, `src/lib/ViewsScreen.svelte`, `src/lib/SettingsScreen.svelte`, `src/lib/RunScreen.svelte`, `src/lib/DataScreen.svelte`.

---

### Task 1: Migration 0007 — quality severity, per-field QC config, verify schedule columns

**Files:**
- Create: `src-tauri/migrations/0007_quality_and_verify.sql`
- Test: `src-tauri/tests/quality.rs` (new file, schema-assertion tests only in this task)

**Interfaces:**
- Produces: `ingest_issue.severity` accepts `'quality'`; `field_def.qc_nonpositive BOOLEAN NOT NULL DEFAULT FALSE`, `field_def.qc_outlier_pct DOUBLE PRECISION NULL`, `field_def.qc_stale_days INTEGER NULL`; `schedule.verify_dow SMALLINT DEFAULT 5` (1=Mon..7=Sun, NULL=off), `schedule.last_verified_on DATE NULL`.

- [ ] **Step 1: Write the failing schema tests**

Create `src-tauri/tests/quality.rs`:

```rust
mod common;

use common::uniq;

#[tokio::test]
#[ignore = "requires postgres"]
async fn severity_quality_is_accepted_and_bogus_is_not() {
    let pool = common::pool().await;
    sqlx::query(
        "INSERT INTO ingest_issue (severity, code, detail)
         VALUES ('quality','quality_test','schema check')")
        .execute(&pool).await.expect("'quality' must pass the severity CHECK");
    let err = sqlx::query(
        "INSERT INTO ingest_issue (severity, code, detail)
         VALUES ('bogus','x','y')")
        .execute(&pool).await;
    assert!(err.is_err(), "unknown severities must still be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn field_def_qc_columns_default_to_disabled() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("QCCLS")).fetch_one(&pool).await.unwrap();
    let (nonpos, outlier, stale): (bool, Option<f64>, Option<i32>) = sqlx::query_as(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'t','numeric')
         RETURNING qc_nonpositive, qc_outlier_pct, qc_stale_days")
        .bind(class).bind(uniq("QCF")).fetch_one(&pool).await.unwrap();
    assert_eq!((nonpos, outlier, stale), (false, None, None),
               "every check is off unless the user turns it on");
    let bad = sqlx::query(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind, qc_outlier_pct)
         VALUES ($1,$2,'t','numeric',-5)")
        .bind(class).bind(uniq("QCB")).execute(&pool).await;
    assert!(bad.is_err(), "a negative outlier threshold is meaningless");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_verify_dow_defaults_to_friday() {
    let pool = common::pool().await;
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("qsched")).fetch_one(&pool).await.unwrap();
    let (dow, last): (Option<i16>, Option<chrono::NaiveDate>) = sqlx::query_as(
        "INSERT INTO schedule (view_id) VALUES ($1) RETURNING verify_dow, last_verified_on")
        .bind(vid).fetch_one(&pool).await.unwrap();
    assert_eq!((dow, last), (Some(5), None));
    let bad = sqlx::query("UPDATE schedule SET verify_dow = 9 WHERE view_id = $1")
        .bind(vid).execute(&pool).await;
    assert!(bad.is_err(), "verify_dow is an ISO weekday, 1-7 or NULL");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run (from `src-tauri/`): `cargo test --test quality -- --ignored`
Expected: FAIL — `'quality'` violates the current severity CHECK; `qc_nonpositive`/`verify_dow` columns do not exist.

- [ ] **Step 3: Write the migration**

Create `src-tauri/migrations/0007_quality_and_verify.sql`:

```sql
-- P7: the quality gate and the weekly verification re-fetch.
--
-- 'quality' is a third severity, distinct on purpose: 'warn' means something
-- did not arrive, 'error' means something failed, 'quality' means a value
-- ARRIVED cleanly and still looks wrong (non-positive price, outlier jump,
-- frozen series, unexplained silence). A reader triaging a run needs the
-- distinction: 'quality' rows point at data to distrust, not plumbing to fix.
ALTER TABLE ingest_issue DROP CONSTRAINT ingest_issue_severity_check;
ALTER TABLE ingest_issue ADD CONSTRAINT ingest_issue_severity_check
  CHECK (severity IN ('warn','error','quality'));

-- Per-field quality thresholds. Data-driven like the rest of field_def
-- (adding a check stays an UPDATE, never a code change), and opt-in per
-- field because applicability depends on what the field IS: a price is
-- never negative, a yield or a spread legitimately is; an FX fix moving 30%
-- is a broken tape, an equity moving 30% is a Tuesday in small caps.
--   qc_nonpositive: flag value_num <= 0.
--   qc_outlier_pct: flag |day-over-day move| above this percentage.
--   qc_stale_days:  flag a value repeated this many consecutive observations.
ALTER TABLE field_def ADD COLUMN qc_nonpositive BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE field_def ADD COLUMN qc_outlier_pct DOUBLE PRECISION
  CONSTRAINT field_def_qc_outlier_positive
  CHECK (qc_outlier_pct IS NULL OR qc_outlier_pct > 0);
ALTER TABLE field_def ADD COLUMN qc_stale_days INTEGER
  CONSTRAINT field_def_qc_stale_min
  CHECK (qc_stale_days IS NULL OR qc_stale_days >= 2);

-- Weekly verification re-fetch: on this ISO weekday (1=Mon..7=Sun, NULL=off)
-- the scheduled run covers the trailing 5 weekdays instead of one, so an
-- upstream restatement is actually re-read -- ingest supersedes the old row
-- and (P7) says so. Defaults ON (Friday): a restatement detector that ships
-- disabled detects nothing.
ALTER TABLE schedule ADD COLUMN verify_dow SMALLINT DEFAULT 5
  CONSTRAINT schedule_verify_dow_range
  CHECK (verify_dow IS NULL OR verify_dow BETWEEN 1 AND 7);
ALTER TABLE schedule ADD COLUMN last_verified_on DATE;
```

- [ ] **Step 4: Verify LF endings, run tests to verify they pass**

Run: `git add src-tauri/migrations/0007_quality_and_verify.sql && git ls-files --eol src-tauri/migrations`
Expected: the new file shows `w/lf` (or `i/lf`); if not, fix `.gitattributes` coverage before proceeding.
Run: `cargo test --test quality -- --ignored`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/migrations/0007_quality_and_verify.sql src-tauri/tests/quality.rs
git commit -m "feat(db): quality severity, per-field QC thresholds, verify schedule (migration 0007)"
```

---

### Task 2: field_def QC plumbing — Rust, command, api.ts, Views screen

**Files:**
- Modify: `src-tauri/src/fields.rs`
- Modify: `src-tauri/src/commands.rs:218-233` (`create_field` command)
- Modify: `src/lib/api.ts` (FieldDef interface + `createField`)
- Modify: `src/lib/ViewsScreen.svelte` (field creation form)
- Test: `src-tauri/tests/quality.rs` (append), inline unit test in `fields.rs`

**Interfaces:**
- Produces: `FieldDef { …existing…, qc_nonpositive: bool, qc_outlier_pct: Option<f64>, qc_stale_days: Option<i32> }`; `fields::create_field(pool, asset_class_id, mnemonic, label, value_kind, bbg_ftype, bbg_datatype, entitlement_note, qc_nonpositive: bool, qc_outlier_pct: Option<f64>, qc_stale_days: Option<i32>) -> AppResult<FieldDef>`; `fields::validate_qc(value_kind, qc_nonpositive, qc_outlier_pct, qc_stale_days) -> AppResult<()>`.
- Consumes: Task 1's columns.

- [ ] **Step 1: Write the failing tests**

In `src-tauri/src/fields.rs` tests module, add:

```rust
    #[test]
    fn qc_thresholds_are_numeric_only() {
        assert!(validate_qc("numeric", true, Some(30.0), Some(5)).is_ok());
        assert!(validate_qc("numeric", false, None, None).is_ok());
        assert!(validate_qc("text", false, None, None).is_ok());
        assert!(validate_qc("text", true, None, None).is_err());
        assert!(validate_qc("date", false, Some(30.0), None).is_err());
        assert!(validate_qc("text", false, None, Some(5)).is_err());
    }
```

In `src-tauri/tests/quality.rs`, append:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn create_field_persists_qc_config() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("QCP")).fetch_one(&pool).await.unwrap();
    let f = getbloomdata_lib::fields::create_field(
        &pool, class, &uniq("px"), "Last", "numeric",
        None, None, "", true, Some(30.0), Some(5)).await.unwrap();
    assert!(f.qc_nonpositive);
    assert_eq!(f.qc_outlier_pct, Some(30.0));
    assert_eq!(f.qc_stale_days, Some(5));
    let err = getbloomdata_lib::fields::create_field(
        &pool, class, &uniq("nm"), "Name", "text",
        None, None, "", true, None, None).await;
    assert!(err.is_err(), "QC on a text field is a config mistake, said early");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p getbloomdata qc_thresholds` → FAIL (`validate_qc` not defined).
Run: `cargo test --test quality create_field_persists -- --ignored` → FAIL to compile (wrong arity).

- [ ] **Step 3: Implement**

In `src-tauri/src/fields.rs`, extend `FieldDef` (after `active: bool`):

```rust
    /// P7 quality gate, all opt-in and numeric-only (validate_qc):
    /// flag <= 0 values / day-over-day moves above this % / a value
    /// repeated this many consecutive observations.
    pub qc_nonpositive: bool,
    pub qc_outlier_pct: Option<f64>,
    pub qc_stale_days: Option<i32>,
```

Add the validator and extend `create_field`:

```rust
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
```

`create_field` gains the three parameters (after `entitlement_note: &str`): `qc_nonpositive: bool, qc_outlier_pct: Option<f64>, qc_stale_days: Option<i32>`; calls `validate_qc(value_kind, qc_nonpositive, qc_outlier_pct, qc_stale_days)?;` after `validate_value_kind`, and the INSERT becomes:

```rust
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                bbg_ftype, bbg_datatype, entitlement_note,
                                qc_nonpositive, qc_outlier_pct, qc_stale_days)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) RETURNING *",
```

with `.bind(qc_nonpositive).bind(qc_outlier_pct).bind(qc_stale_days)` appended.

In `src-tauri/src/commands.rs`, `create_field` command gains `qc_nonpositive: Option<bool>, qc_outlier_pct: Option<f64>, qc_stale_days: Option<i32>` and passes `qc_nonpositive.unwrap_or(false), qc_outlier_pct, qc_stale_days`.

In `src/lib/api.ts`: `FieldDef` gains `qc_nonpositive: boolean; qc_outlier_pct: number | null; qc_stale_days: number | null;` and `createField` gains trailing params `qcNonpositive: boolean = false, qcOutlierPct: number | null = null, qcStaleDays: number | null = null`, passed in the invoke payload as `qcNonpositive, qcOutlierPct, qcStaleDays`.

In `src/lib/ViewsScreen.svelte`: extend `newField` state with `qc_nonpositive: false, qc_outlier_pct: "", qc_stale_days: ""`; in `addField()` pass `newField.qc_nonpositive, newField.qc_outlier_pct === "" ? null : Number(newField.qc_outlier_pct), newField.qc_stale_days === "" ? null : Number(newField.qc_stale_days)` and reset them with the other fields. In the field form (after the entitlement-note input, before the submit button) add:

```svelte
    <label class="check" title="Quality gate: flag values ≤ 0 (numeric fields only)">
      <input type="checkbox" bind:checked={newField.qc_nonpositive} /> flag ≤ 0
    </label>
    <input type="number" bind:value={newField.qc_outlier_pct} min="0" step="any"
           placeholder="outlier % (optional)" title="Quality gate: flag day-over-day moves above this %" />
    <input type="number" bind:value={newField.qc_stale_days} min="2"
           placeholder="stale after N (optional)" title="Quality gate: flag a value repeated N consecutive observations" />
```

and add `.check { flex-direction: row; gap: 0.3rem; align-items: center; }` scoped style if the form labels are column-flex (they are: `form label { flex-direction: column; }` — add `form label.check { flex-direction: row; align-items: center; }`).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p getbloomdata qc_thresholds` → PASS.
Run: `cargo test --test quality -- --ignored` → PASS.
Run (repo root): `npm run check` → no new errors.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/fields.rs src-tauri/src/commands.rs src-tauri/tests/quality.rs src/lib/api.ts src/lib/ViewsScreen.svelte
git commit -m "feat: per-field quality thresholds -- config plumbing end to end"
```

---

### Task 3: quality.rs — pure checks

**Files:**
- Create: `src-tauri/src/quality.rs` (pure half + inline tests)
- Modify: `src-tauri/src/lib.rs` (add `pub mod quality;` in alphabetical position, after `pub mod orchestrator;`)

**Interfaces:**
- Produces:
  - `pub struct QcConfig { pub nonpositive: bool, pub outlier_pct: Option<f64>, pub stale_days: Option<i32> }` with `pub fn enabled(&self) -> bool`
  - `pub struct SeriesFinding { pub obs_date: chrono::NaiveDate, pub code: &'static str, pub detail: String }`
  - `pub fn evaluate_series(cfg: &QcConfig, series: &[(chrono::NaiveDate, f64)], from: chrono::NaiveDate, to: chrono::NaiveDate) -> Vec<SeriesFinding>` — `series` ascending by date, findings only for dates in `[from, to]`
  - `pub fn unexplained_instruments(requested: &[i64], outcome: &crate::fetch::FetchOutcome) -> Vec<i64>`
- Codes emitted: `quality_not_finite` (unconditional), `quality_nonpositive`, `quality_outlier`, `quality_stale`.

- [ ] **Step 1: Write the module with failing tests first**

Create `src-tauri/src/quality.rs` with this skeleton — write the tests module first, stub `evaluate_series` to return `vec![]` and `unexplained_instruments` to return `vec![]`:

```rust
//! P7: the quality gate. Structural validation (types, dates, security
//! errors) already lives in the sidecar and fetch::coerce; this module is
//! the missing judgment layer -- a value that arrived CLEANLY and still
//! looks wrong. Pure functions here; the DB runner lives below them.
//!
//! Every check is per-field opt-in (field_def.qc_*): whether a check makes
//! sense depends on what the field IS, and nothing here guesses from a
//! mnemonic. The one unconditional check is IEEE weirdness (NaN/inf), which
//! is wrong for every numeric field there is.

use chrono::NaiveDate;

#[derive(Debug, Clone, Copy, Default)]
pub struct QcConfig {
    pub nonpositive: bool,
    pub outlier_pct: Option<f64>,
    pub stale_days: Option<i32>,
}

impl QcConfig {
    pub fn enabled(&self) -> bool {
        self.nonpositive || self.outlier_pct.is_some() || self.stale_days.is_some()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SeriesFinding {
    pub obs_date: NaiveDate,
    pub code: &'static str,
    pub detail: String,
}
```

Implementation contract for `evaluate_series` (write after tests):

```rust
/// Walk an ASCENDING current-raw series once; report only for dates inside
/// [from, to] -- the run being judged -- so history is context, not noise.
pub fn evaluate_series(cfg: &QcConfig, series: &[(NaiveDate, f64)],
                       from: NaiveDate, to: NaiveDate) -> Vec<SeriesFinding> {
    let mut out = Vec::new();
    let in_range = |d: NaiveDate| d >= from && d <= to;
    let mut streak = 1usize;
    for (i, &(d, v)) in series.iter().enumerate() {
        let prev = (i > 0).then(|| series[i - 1]);
        if let Some((_, pv)) = prev {
            streak = if pv == v { streak + 1 } else { 1 };
        }
        if !in_range(d) {
            continue;
        }
        if !v.is_finite() {
            out.push(SeriesFinding { obs_date: d, code: "quality_not_finite",
                detail: format!("stored value {v} is not a finite number") });
            continue; // the other checks are meaningless on NaN/inf
        }
        if cfg.nonpositive && v <= 0.0 {
            out.push(SeriesFinding { obs_date: d, code: "quality_nonpositive",
                detail: format!("value {v} is not positive") });
        }
        if let (Some(pct), Some((pd, pv))) = (cfg.outlier_pct, prev) {
            if pv != 0.0 && pv.is_finite() {
                let mv = (v / pv - 1.0) * 100.0;
                if mv.abs() > pct {
                    out.push(SeriesFinding { obs_date: d, code: "quality_outlier",
                        detail: format!("moved {mv:.1}% vs {pd} ({pv} -> {v}), \
                                         threshold {pct}%") });
                }
            }
        }
        if let Some(n) = cfg.stale_days {
            let n = n as usize;
            // Alert when the streak first reaches n, and keep alerting on the
            // newest point while it stays frozen (daily runs see the series
            // end); a backfill over the middle of a long streak stays quiet.
            if streak == n || (streak > n && i == series.len() - 1) {
                out.push(SeriesFinding { obs_date: d, code: "quality_stale",
                    detail: format!("unchanged for {streak} consecutive \
                                     observations (threshold {n})") });
            }
        }
    }
    out
}

/// Instruments the run REQUESTED and Bloomberg answered with silence -- no
/// cell, no problem. A holiday is not silence (it arrives as no_data); this
/// is the partial-response case where a name simply vanished from the reply.
pub fn unexplained_instruments(requested: &[i64],
                               outcome: &crate::fetch::FetchOutcome) -> Vec<i64> {
    use std::collections::HashSet;
    let mut explained: HashSet<i64> = outcome.cells.iter()
        .map(|c| c.instrument_id).collect();
    explained.extend(outcome.problems.iter().filter_map(|p| p.instrument_id));
    requested.iter().copied().filter(|id| !explained.contains(id)).collect()
}
```

Inline tests to write FIRST (stub bodies, watch them fail, then paste the implementations above):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::fetch::{CellProblem, CellValue, FetchOutcome, ObsCell};

    fn d(s: &str) -> NaiveDate { s.parse().unwrap() }
    fn cfg(nonpos: bool, outlier: Option<f64>, stale: Option<i32>) -> QcConfig {
        QcConfig { nonpositive: nonpos, outlier_pct: outlier, stale_days: stale }
    }

    #[test]
    fn nonpositive_and_not_finite_are_flagged_in_range_only() {
        let s = [(d("2026-08-10"), -1.0), (d("2026-08-11"), 0.0),
                 (d("2026-08-12"), f64::NAN), (d("2026-08-13"), 10.0)];
        let f = evaluate_series(&cfg(true, None, None), &s,
                                d("2026-08-11"), d("2026-08-13"));
        // the 08-10 value is out of range; 08-11 nonpositive; 08-12 not finite
        assert_eq!(f.len(), 2);
        assert_eq!((f[0].obs_date, f[0].code), (d("2026-08-11"), "quality_nonpositive"));
        assert_eq!((f[1].obs_date, f[1].code), (d("2026-08-12"), "quality_not_finite"));
    }

    #[test]
    fn outlier_compares_against_the_previous_observation() {
        let s = [(d("2026-08-10"), 100.0), (d("2026-08-11"), 100.5),
                 (d("2026-08-12"), 145.0)];
        let f = evaluate_series(&cfg(false, Some(30.0), None), &s,
                                d("2026-08-12"), d("2026-08-12"));
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].code, "quality_outlier");
        assert!(f[0].detail.contains("44.3%"), "detail: {}", f[0].detail);
        // a 30% threshold is not tripped by 0.5%
        let quiet = evaluate_series(&cfg(false, Some(30.0), None), &s,
                                    d("2026-08-11"), d("2026-08-11"));
        assert!(quiet.is_empty());
    }

    #[test]
    fn outlier_skips_a_zero_previous_value() {
        let s = [(d("2026-08-11"), 0.0), (d("2026-08-12"), 5.0)];
        assert!(evaluate_series(&cfg(false, Some(30.0), None), &s,
                                d("2026-08-12"), d("2026-08-12")).is_empty());
    }

    #[test]
    fn stale_fires_when_the_streak_reaches_n_and_on_the_frozen_series_end() {
        let s = [(d("2026-08-10"), 7.0), (d("2026-08-11"), 7.0),
                 (d("2026-08-12"), 7.0), (d("2026-08-13"), 7.0)];
        // streak hits 3 on 08-12
        let at_n = evaluate_series(&cfg(false, None, Some(3)), &s,
                                   d("2026-08-12"), d("2026-08-12"));
        assert_eq!(at_n.len(), 1);
        assert_eq!(at_n[0].code, "quality_stale");
        // the next daily run (range = 08-13 only, streak 4 > n at series end)
        let next_day = evaluate_series(&cfg(false, None, Some(3)), &s,
                                       d("2026-08-13"), d("2026-08-13"));
        assert_eq!(next_day.len(), 1, "a still-frozen series keeps alarming");
        // a varied series never fires
        let varied = [(d("2026-08-10"), 7.0), (d("2026-08-11"), 7.1),
                      (d("2026-08-12"), 7.0)];
        assert!(evaluate_series(&cfg(false, None, Some(2)), &varied,
                                d("2026-08-10"), d("2026-08-12")).is_empty());
    }

    #[test]
    fn unexplained_silence_is_requested_minus_cells_minus_problems() {
        let out = FetchOutcome {
            cells: vec![ObsCell { instrument_id: 1, field_id: 9,
                                  obs_date: d("2026-08-12"),
                                  value: CellValue::Num(1.0) }],
            problems: vec![CellProblem { instrument_id: Some(2), field_id: None,
                                         obs_date: Some(d("2026-08-12")),
                                         code: "no_data".into(),
                                         detail: "holiday".into() }],
        };
        assert_eq!(unexplained_instruments(&[1, 2, 3], &out), vec![3]);
        assert!(unexplained_instruments(&[1, 2], &out).is_empty());
    }
}
```

NOTE: check `crate::fetch::CellProblem`'s exact field types before writing the test (open `src-tauri/src/fetch.rs`; `code`/`detail` may be `String`, construct accordingly). Adjust the struct literal to compile against the real definition — the semantic assertions stay as written.

- [ ] **Step 2: Run tests to verify they fail** — `cargo test -p getbloomdata quality::` → FAIL with stubs.

- [ ] **Step 3: Paste the real implementations** (bodies shown above).

- [ ] **Step 4: Run tests to verify they pass** — `cargo test -p getbloomdata quality::` → PASS (5 tests). Note: the outlier detail assertion expects `44.3%` (145/100.5 − 1 = 44.28%); if rounding differs, fix the expectation to the actual one-decimal rendering, not the code.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/quality.rs src-tauri/src/lib.rs
git commit -m "feat: P7 pure quality checks -- nonpositive, outlier, stale, unexplained silence"
```

---

### Task 4: quality gate runner + orchestrator wiring + run surface

**Files:**
- Modify: `src-tauri/src/quality.rs` (DB half)
- Modify: `src-tauri/src/orchestrator.rs` (`RunOutcome::Completed` gains `quality_findings: u64`; `execute` runs the gate)
- Modify: `src-tauri/src/scheduler.rs:155-161` (match arm + message)
- Modify: `src/lib/api.ts` (`RunOutcome` type), `src/lib/RunScreen.svelte` (quality line)
- Test: `src-tauri/tests/quality.rs` (append)

**Interfaces:**
- Produces: `quality::run_quality_gate(pool: &PgPool, run_id: i64, req: &crate::fetch::FetchRequest, outcome: &crate::fetch::FetchOutcome) -> AppResult<u64>`; `RunOutcome::Completed { run_id, summary, corp_actions, quality_findings: u64 }`.
- Consumes: Task 2's `field_def.qc_*` columns, Task 3's pure functions.

- [ ] **Step 1: Write the failing integration test**

Append to `src-tauri/tests/quality.rs`:

```rust
use chrono::NaiveDate;
use getbloomdata_lib::fetch::{CellValue, FetchAsset, FetchField, FetchOutcome,
                              FetchRequest, ObsCell};
use getbloomdata_lib::{ingest, quality};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// Instrument + numeric field with QC on + view + run; returns ids.
async fn qc_scaffold(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                qc_nonpositive, qc_outlier_pct, qc_stale_days)
         VALUES ($1,$2,'Last','numeric',true,30,3) RETURNING id")
        .bind(class).bind(uniq("PXQ")).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("qgv")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','fetching') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (iid, fid, rid)
}

fn req_for(rid: i64, iid: i64, fid: i64, class: i64,
           start: NaiveDate, end: NaiveDate) -> FetchRequest {
    FetchRequest {
        run_id: rid,
        assets: vec![FetchAsset { instrument_id: iid, asset_class_id: class,
                                  class_name: "c".into(), label: "l".into(),
                                  bdp_security: "X US Equity".into() }],
        fields: vec![FetchField { field_id: fid, asset_class_id: class,
                                  mnemonic: "PX_LAST".into(),
                                  value_kind: "numeric".into() }],
        start, end,
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_gate_writes_quality_issues_for_flagged_values() {
    let pool = common::pool().await;
    let (iid, fid, rid) = qc_scaffold(&pool, "QGATE").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM field_def WHERE id = $1")
        .bind(fid).fetch_one(&pool).await.unwrap();
    // Day 1 at 100, day 2 at 145 (outlier vs 30%), day 3 at -2 (nonpositive).
    let cells = vec![
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-11"), value: CellValue::Num(100.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-12"), value: CellValue::Num(145.0) },
        ObsCell { instrument_id: iid, field_id: fid,
                  obs_date: d("2026-08-13"), value: CellValue::Num(-2.0) },
    ];
    let outcome = FetchOutcome { cells, problems: vec![] };
    ingest::ingest_outcome(&pool, rid, &outcome).await.unwrap();
    let req = req_for(rid, iid, fid, class, d("2026-08-11"), d("2026-08-13"));
    let n = quality::run_quality_gate(&pool, rid, &req, &outcome).await.unwrap();
    assert!(n >= 2, "outlier + nonpositive at minimum, got {n}");
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT severity, code FROM ingest_issue
          WHERE run_id = $1 AND severity = 'quality' ORDER BY code")
        .bind(rid).fetch_all(&pool).await.unwrap();
    let codes: Vec<&str> = rows.iter().map(|(_, c)| c.as_str()).collect();
    assert!(codes.contains(&"quality_outlier"), "codes: {codes:?}");
    assert!(codes.contains(&"quality_nonpositive"), "codes: {codes:?}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn unexplained_silence_becomes_quality_no_response() {
    let pool = common::pool().await;
    let (iid, fid, rid) = qc_scaffold(&pool, "QSIL").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM field_def WHERE id = $1")
        .bind(fid).fetch_one(&pool).await.unwrap();
    // Requested, but the outcome mentions it nowhere.
    let outcome = FetchOutcome { cells: vec![], problems: vec![] };
    let req = req_for(rid, iid, fid, class, d("2026-08-13"), d("2026-08-13"));
    let n = quality::run_quality_gate(&pool, rid, &req, &outcome).await.unwrap();
    assert_eq!(n, 1);
    let code: String = sqlx::query_scalar(
        "SELECT code FROM ingest_issue
          WHERE run_id = $1 AND instrument_id = $2 AND severity = 'quality'")
        .bind(rid).bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(code, "quality_no_response");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --test quality -- --ignored` → FAIL to compile (`run_quality_gate` missing).

- [ ] **Step 3: Implement the runner**

Append to `src-tauri/src/quality.rs`:

```rust
// ---------------------------------------------------------------- DB runner

use crate::error::AppResult;
use crate::fetch::{FetchOutcome, FetchRequest};
use sqlx::PgPool;

/// Judge a run AFTER ingest committed, against what the database now holds:
/// the stored series is the single source the checks read, so a backfill and
/// an EOD run are judged identically. Findings are ingest_issue rows with
/// severity 'quality', attached to the run. Advisory by contract -- the
/// caller logs an error and keeps the run.
pub async fn run_quality_gate(pool: &PgPool, run_id: i64, req: &FetchRequest,
                              outcome: &FetchOutcome) -> AppResult<u64> {
    let mut findings = 0u64;

    let requested: Vec<i64> = req.assets.iter().map(|a| a.instrument_id).collect();
    for iid in unexplained_instruments(&requested, outcome) {
        sqlx::query(
            "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
             VALUES ($1,$2,'quality','quality_no_response',
                     'requested in this run but Bloomberg returned neither data \
                      nor a problem for it')")
            .bind(run_id).bind(iid).execute(pool).await?;
        findings += 1;
    }

    // Which fields carry any check at all -- one query, not one per cell.
    let mut field_ids: Vec<i64> = outcome.cells.iter().map(|c| c.field_id).collect();
    field_ids.sort_unstable();
    field_ids.dedup();
    if field_ids.is_empty() {
        return Ok(findings);
    }
    let cfgs: Vec<(i64, bool, Option<f64>, Option<i32>)> = sqlx::query_as(
        "SELECT id, qc_nonpositive, qc_outlier_pct, qc_stale_days
           FROM field_def WHERE id = ANY($1)")
        .bind(&field_ids).fetch_all(pool).await?;
    let cfg_of = |fid: i64| cfgs.iter()
        .find(|(id, ..)| *id == fid)
        .map(|&(_, n, o, s)| QcConfig { nonpositive: n, outlier_pct: o, stale_days: s })
        .unwrap_or_default();

    let mut pairs: Vec<(i64, i64)> = outcome.cells.iter()
        .map(|c| (c.instrument_id, c.field_id)).collect();
    pairs.sort_unstable();
    pairs.dedup();

    for (iid, fid) in pairs {
        let cfg = cfg_of(fid);
        if !cfg.enabled() {
            continue;
        }
        // Enough history for the stale streak plus the run's own range; the
        // series is judged ascending, so the DESC page is reversed.
        let span = (req.end - req.start).num_days().max(0) as i64;
        let window = (cfg.stale_days.unwrap_or(0) as i64 + span + 10).clamp(10, 200);
        let mut series: Vec<(chrono::NaiveDate, f64)> = sqlx::query_as(
            "SELECT obs_date, value_num FROM observation
              WHERE instrument_id = $1 AND field_id = $2
                AND layer = 'raw' AND granularity = 'eod'
                AND system_to = 'infinity' AND value_num IS NOT NULL
                AND obs_date <= $3
              ORDER BY obs_date DESC LIMIT $4")
            .bind(iid).bind(fid).bind(req.end).bind(window)
            .fetch_all(pool).await?;
        series.reverse();
        for f in evaluate_series(&cfg, &series, req.start, req.end) {
            sqlx::query(
                "INSERT INTO ingest_issue
                   (run_id, instrument_id, field_id, obs_date, severity, code, detail)
                 VALUES ($1,$2,$3,$4,'quality',$5,$6)")
                .bind(run_id).bind(iid).bind(fid).bind(f.obs_date)
                .bind(f.code).bind(&f.detail)
                .execute(pool).await?;
            findings += 1;
        }
    }
    Ok(findings)
}
```

- [ ] **Step 4: Wire into the orchestrator**

In `src-tauri/src/orchestrator.rs`:

1. `RunOutcome::Completed` gains a field (after `corp_actions`):
```rust
        /// P7: ingest_issue rows with severity 'quality' this run produced.
        /// Anything above zero makes the run 'partial' -- a number that
        /// arrived cleanly but looks wrong is still a reason to look.
        quality_findings: u64,
```
2. In `execute` (`orchestrator.rs:262-279`), after the `summary` binding and before the `status` line:
```rust
    // P7 quality gate: judged against what the database now holds. Advisory
    // like its siblings -- a gate failure must not fail a run that ingested.
    let quality_findings = match crate::quality::run_quality_gate(
        pool, run_id, &req, &outcome).await {
        Ok(n) => n,
        Err(e) => {
            eprintln!("warning: quality gate failed for run {run_id}: {e}");
            0
        }
    };

    let status = if summary.issues > 0 || quality_findings > 0 { "partial" } else { "ok" };
```
and the return becomes `Ok(RunOutcome::Completed { run_id, summary, corp_actions: None, quality_findings })`.
3. Fix every `RunOutcome::Completed { ... }` pattern the compiler reports: in this file (`corp_actions_after` at `:323` uses `..` already — verify), in `scheduler.rs:155` change the arm to

```rust
            Ok(RunOutcome::Completed { run_id, summary, corp_actions,
                                       quality_findings }) => {
                let ca = match corp_actions {
                    Some(c) => format!(" ca_new={} ca_amended={}", c.inserted, c.amended),
                    None => String::new(),
                };
                let q = if *quality_findings > 0 {
                    format!(" quality={quality_findings}")
                } else { String::new() };
                format!("ok run={run_id} inserted={} superseded={} issues={}{q}{ca}",
                        summary.inserted, summary.superseded, summary.issues)
            }
```
and run `cargo build 2>&1 | grep -A2 "Completed"` to find any remaining sites (integration tests included — add `quality_findings: _` or assert `== 0` where the mock data is clean, e.g. `tests/pipeline.rs`, `tests/db_integration.rs` if they destructure).

4. `src/lib/api.ts` — `RunOutcome`'s `Completed` object gains `quality_findings: number;`.
5. `src/lib/RunScreen.svelte` — add a quality line: new state `let qualityLine = $state("");` next to `caLine`; extend `noteCorpActions` (it already receives the whole outcome):
```ts
    const q = "Completed" in outcome ? outcome.Completed.quality_findings : 0;
    qualityLine = q ? `⚠ ${q} quality finding(s) — click the run below to see them.` : "";
```
and render after the `caLine` paragraph (`RunScreen.svelte:169`):
```svelte
  {#if qualityLine}<p class="amber">{qualityLine}</p>{/if}
```

- [ ] **Step 5: Run all tests**

`cargo test` → unit suites PASS. `cargo test -- --ignored` → PASS including the two new gate tests. `npm run check` → clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/quality.rs src-tauri/src/orchestrator.rs src-tauri/src/scheduler.rs src-tauri/tests src/lib/api.ts src/lib/RunScreen.svelte
git commit -m "feat: P7 quality gate rides every run; findings make it partial and are shown"
```

---

### Task 5: supersession alerts in ingest

**Files:**
- Modify: `src-tauri/src/ingest.rs:61-69` (supersede branch)
- Test: `src-tauri/tests/quality.rs` (append)

**Interfaces:**
- Produces: an `ingest_issue` row `(run_id, instrument_id, field_id, obs_date, 'warn', 'value_superseded', detail)` for every superseded observation. `IngestSummary` semantics unchanged (`issues` still counts fetch problems only; run status is untouched by supersession — a correction is legitimate, it just must be visible).

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/quality.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_superseded_value_leaves_a_visible_issue_and_unchanged_does_not() {
    let pool = common::pool().await;
    let (iid, fid, rid) = qc_scaffold(&pool, "QSUP").await;
    let cell = |v: f64| FetchOutcome {
        cells: vec![ObsCell { instrument_id: iid, field_id: fid,
                              obs_date: d("2026-08-13"),
                              value: CellValue::Num(v) }],
        problems: vec![],
    };
    ingest::ingest_outcome(&pool, rid, &cell(101.5)).await.unwrap();
    // Same value again: no supersession, no issue.
    let s2 = ingest::ingest_outcome(&pool, rid, &cell(101.5)).await.unwrap();
    assert_eq!((s2.superseded, s2.unchanged), (0, 1));
    // Restated value: superseded + a value_superseded issue naming both numbers.
    let s3 = ingest::ingest_outcome(&pool, rid, &cell(99.75)).await.unwrap();
    assert_eq!(s3.superseded, 1);
    let details: Vec<String> = sqlx::query_scalar(
        "SELECT detail FROM ingest_issue
          WHERE run_id = $1 AND code = 'value_superseded'")
        .bind(rid).fetch_all(&pool).await.unwrap();
    assert_eq!(details.len(), 1, "one alert for one restatement");
    assert!(details[0].contains("101.5") && details[0].contains("99.75"),
            "detail must name old and new: {}", details[0]);
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --test quality a_superseded -- --ignored` → FAIL (no issue rows).

- [ ] **Step 3: Implement**

In `src-tauri/src/ingest.rs`, replace the supersede branch (lines 61-69) with:

```rust
        if let Some((id, old_num, old_text)) = current {
            if old_num == num && old_text == text {
                unchanged += 1;
                continue;
            }
            sqlx::query("UPDATE observation SET system_to = now() WHERE id = $1")
                .bind(id).execute(&mut *tx).await?;
            // A restatement is legitimate -- and invisible unless said. The
            // run stays ok/partial on its own merits; this row is the audit
            // trail's headline, not a failure.
            let describe = |n: &Option<f64>, t: &Option<String>| match (n, t) {
                (Some(v), _) => v.to_string(),
                (_, Some(s)) => format!("{s:?}"),
                _ => "NULL".into(),
            };
            sqlx::query(
                "INSERT INTO ingest_issue
                   (run_id, instrument_id, field_id, obs_date, severity, code, detail)
                 VALUES ($1,$2,$3,$4,'warn','value_superseded',$5)")
                .bind(run_id).bind(c.instrument_id).bind(c.field_id).bind(c.obs_date)
                .bind(format!("stored value {} superseded by {}",
                              describe(&old_num, &old_text), describe(&num, &text)))
                .execute(&mut *tx).await?;
            superseded += 1;
        }
```

- [ ] **Step 4: Run tests** — `cargo test --test quality -- --ignored` → PASS. Also run `cargo test --test pipeline -- --ignored` and `cargo test --test db_integration -- --ignored`: if any existing test counts `ingest_issue` rows after a supersession, update its expectation and say so in the commit body.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/ingest.rs src-tauri/tests/quality.rs
git commit -m "feat: restatements announce themselves -- value_superseded issue on every close"
```

---

### Task 6: weekly verification re-fetch

**Files:**
- Modify: `src-tauri/src/scheduler.rs` (pure helpers, `due_schedules`, `already_ran_today`, `tick`)
- Modify: `src-tauri/src/orchestrator.rs` (`run_verify` / `run_verify_with`)
- Modify: `src-tauri/src/commands.rs` (`upsert_schedule`, `ScheduleRow`, `list_schedules`)
- Modify: `src/lib/api.ts` (`ScheduleRow`, `upsertSchedule`), `src/lib/SettingsScreen.svelte`
- Test: inline in `scheduler.rs`; DB tests appended to `src-tauri/tests/quality.rs`

**Interfaces:**
- Produces: `scheduler::iso_dow(d: NaiveDate) -> i16` (1=Mon..7=Sun); `scheduler::verify_window_start(end: NaiveDate) -> NaiveDate` (4 more weekdays back → a 5-weekday window ending at `end`); `orchestrator::run_verify(pool, cfg, view_id, start, end) -> AppResult<RunOutcome>` and `run_verify_with<F: DataFetcher>(pool, cfg, fetcher, view_id, start, end)` — a backfill with trigger `'scheduled'`, gated at `HardConfirm` (returns `NeedsConfirmation` instead of running; never asks for a click).
- Consumes: Task 1's `schedule.verify_dow`/`last_verified_on`; Task 4's `RunOutcome` shape.

- [ ] **Step 1: Write the failing pure tests**

In `src-tauri/src/scheduler.rs` tests module, add:

```rust
    #[test]
    fn verify_window_is_five_weekdays_ending_at_end() {
        // Friday 2026-08-14 back to Monday 2026-08-10
        assert_eq!(verify_window_start(NaiveDate::from_ymd_opt(2026, 8, 14).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 10).unwrap());
        // Monday 2026-08-17 back across the weekend to Tuesday 2026-08-11
        assert_eq!(verify_window_start(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()),
                   NaiveDate::from_ymd_opt(2026, 8, 11).unwrap());
    }

    #[test]
    fn iso_dow_is_monday_one_sunday_seven() {
        assert_eq!(iso_dow(NaiveDate::from_ymd_opt(2026, 8, 17).unwrap()), 1); // Mon
        assert_eq!(iso_dow(NaiveDate::from_ymd_opt(2026, 8, 21).unwrap()), 5); // Fri
        assert_eq!(iso_dow(NaiveDate::from_ymd_opt(2026, 8, 16).unwrap()), 7); // Sun
    }
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p getbloomdata verify_window` → FAIL (missing fns).

- [ ] **Step 3: Implement scheduler helpers + orchestrator entry**

`src-tauri/src/scheduler.rs`, after `previous_weekday`:

```rust
/// ISO weekday, matching schedule.verify_dow: 1 = Monday .. 7 = Sunday.
pub fn iso_dow(d: NaiveDate) -> i16 {
    d.weekday().number_from_monday() as i16
}

/// The verify run covers the trailing five weekdays: `end` plus four more
/// weekdays back. One week of history is enough to catch the common
/// restatement (yesterday's close corrected today) without pricing a
/// five-fold budget surprise into every single day.
pub fn verify_window_start(end: NaiveDate) -> NaiveDate {
    let mut d = end;
    for _ in 0..4 {
        d = previous_weekday(d);
    }
    d
}
```

`already_ran_today` (`scheduler.rs:44-52`): change the kind filter so a completed verify counts as the day's run — `AND kind IN ('eod','backfill') AND trigger_kind = 'scheduled'` (comment: a scheduled verify backfill IS that day's scheduled run; without this the EOD run would fire again an hour later and double-charge the day). Leave `failed_attempts_today` on `kind = 'eod'`? No — apply the same `kind IN ('eod','backfill')` there too, so three failed verify attempts also stop the day.

`due_schedules` (`scheduler.rs:89-95`): select two more columns and widen the tuple:

```rust
pub async fn due_schedules(pool: &PgPool)
    -> AppResult<Vec<(i64, i64, Option<String>, Option<i16>, Option<NaiveDate>)>> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.view_id, s.last_result, s.verify_dow, s.last_verified_on
         FROM schedule s JOIN view v ON v.id = s.view_id
         WHERE s.active AND v.active")
        .fetch_all(pool).await?)
}
```

`tick` (`scheduler.rs:97-174`): destructure `for (sid, view_id, last_result, verify_dow, last_verified_on) in schedules` and replace the run block (`:150-168`) with:

```rust
        // Amendment A1 stands: the run targets the previous trading day.
        // On the schedule's verify day, the same slot instead re-reads the
        // trailing five weekdays (kind backfill, trigger scheduled) so an
        // upstream restatement is actually seen. Budget-blocked verifies
        // degrade to the normal one-day run rather than blocking the day.
        let obs_date = previous_weekday(today);
        let want_verify = verify_dow == Some(iso_dow(today))
            && last_verified_on.is_none_or(|d| d < today);
        let mut note = String::new();
        let result = if want_verify {
            match orchestrator::run_verify(pool, cfg, view_id,
                                           verify_window_start(obs_date), obs_date).await {
                Ok(RunOutcome::NeedsConfirmation { estimated, .. }) => {
                    note = format!("verify skipped ({estimated} est. hits needs \
                                    confirmation); ");
                    orchestrator::run_eod(pool, cfg, view_id, "scheduled",
                                          obs_date, false).await
                }
                other => {
                    if matches!(other, Ok(RunOutcome::Completed { .. })) {
                        note = "verify ".into();
                        let _ = sqlx::query(
                            "UPDATE schedule SET last_verified_on = $2 WHERE id = $1")
                            .bind(sid).bind(today).execute(pool).await;
                    }
                    other
                }
            }
        } else {
            orchestrator::run_eod(pool, cfg, view_id, "scheduled", obs_date, false).await
        };
        let msg = match &result {
            Ok(RunOutcome::Completed { run_id, summary, corp_actions,
                                       quality_findings }) => {
                let ca = match corp_actions {
                    Some(c) => format!(" ca_new={} ca_amended={}", c.inserted, c.amended),
                    None => String::new(),
                };
                let q = if *quality_findings > 0 {
                    format!(" quality={quality_findings}")
                } else { String::new() };
                format!("{note}ok run={run_id} inserted={} superseded={} issues={}{q}{ca}",
                        summary.inserted, summary.superseded, summary.issues)
            }
            Ok(RunOutcome::NeedsConfirmation { estimated, .. }) =>
                format!("{note}blocked: needs confirmation for {estimated} estimated hits"),
            Err(e) => format!("{note}failed: {e}"),
        };
```

(the trailing `UPDATE schedule SET last_result` and `launched.push` lines stay as they are).

`src-tauri/src/orchestrator.rs`, after `run_backfill_with`:

```rust
/// P7: the weekly verification re-fetch -- a SCHEDULED multi-day backfill
/// over the trailing week, so upstream restatements are re-read and ingest's
/// value_superseded alert has something to bite on. Gated like an EOD run
/// (HardConfirm blocks it -- a scheduler cannot click a confirm box);
/// NeedsConfirmation here means "skip this week's verify", never "ask".
pub async fn run_verify(
    pool: &PgPool,
    cfg: &PipelineConfig,
    view_id: i64,
    start: NaiveDate,
    end: NaiveDate,
) -> AppResult<RunOutcome> {
    let mut result = run_verify_with(pool, cfg, &BlpapiFetcher { cfg }, view_id,
                                     start, end).await;
    auto_reresolve_after(pool, cfg, &result).await;
    corp_actions_after(pool, cfg, view_id, &mut result).await;
    lifecycle_after(pool, cfg, &result).await;
    result
}

pub async fn run_verify_with<F: DataFetcher>(
    pool: &PgPool,
    cfg: &PipelineConfig,
    fetcher: &F,
    view_id: i64,
    start: NaiveDate,
    end: NaiveDate,
) -> AppResult<RunOutcome> {
    if start > end {
        return Err(AppError::Validation("start after end".into()));
    }
    if (end - start).num_days() + 1 > BACKFILL_CAP_DAYS {
        return Err(AppError::Validation(format!(
            "verify range exceeds {BACKFILL_CAP_DAYS}-day cap")));
    }
    let loaded = load_view(pool, view_id, None).await?;
    let estimated = budget::estimate_backfill_hits(&loaded.assets, &loaded.fields, start, end)
        + corp_actions_estimate(pool, view_id).await?;
    let today_total = budget::today_hits(pool).await?;
    if budget::check_level(estimated, today_total, cfg.soft_limit) == BudgetLevel::HardConfirm {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }
    execute(pool, cfg, fetcher, &loaded, view_id, "backfill", "scheduled",
            start, end, estimated).await
}
```

- [ ] **Step 4: Command + UI surface**

`src-tauri/src/commands.rs`:
- `ScheduleRow` gains `pub verify_dow: Option<i16>, pub last_verified_on: Option<chrono::NaiveDate>`; `list_schedules`' SELECT adds `verify_dow, last_verified_on`.
- `upsert_schedule` gains `verify_dow: Option<i16>` and the SQL becomes:
```rust
        "INSERT INTO schedule (view_id, window_start, window_end, active, verify_dow)
         VALUES ($1, $2::time, $3::time, $4, $5)
         ON CONFLICT (view_id) DO UPDATE
           SET window_start = EXCLUDED.window_start,
               window_end = EXCLUDED.window_end,
               active = EXCLUDED.active,
               verify_dow = EXCLUDED.verify_dow,
               drawn_for = NULL, drawn_at = NULL")
```
with `.bind(verify_dow)` appended.

`src/lib/api.ts`: `ScheduleRow` gains `verify_dow: number | null; last_verified_on: string | null;`; `upsertSchedule` becomes `(viewId, windowStart, windowEnd, active, verifyDow: number | null)` passing `verifyDow`.

`src/lib/SettingsScreen.svelte`:
- `newSchedule` gains `verify_dow: 5 as number | null`; `upsert()` passes `newSchedule.verify_dow`; `toggleScheduleActive` passes `s.verify_dow`.
- The add/update form gains, before the Active checkbox:
```svelte
    <label>
      Verify day
      <select bind:value={newSchedule.verify_dow}
              title="Once a week, re-fetch the trailing 5 weekdays so upstream restatements are caught. Off = never.">
        <option value={null}>Off</option>
        <option value={1}>Monday</option>
        <option value={2}>Tuesday</option>
        <option value={3}>Wednesday</option>
        <option value={4}>Thursday</option>
        <option value={5}>Friday</option>
      </select>
    </label>
```
- The schedules table header gains `<th>Verify</th>` (after "Window end") and each row `<td>{s.verify_dow ? ["","Mon","Tue","Wed","Thu","Fri","Sat","Sun"][s.verify_dow] + (s.last_verified_on ? ` (last ${s.last_verified_on})` : "") : "off"}</td>`.

- [ ] **Step 5: Write and run the DB tests**

Append to `src-tauri/tests/quality.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_scheduled_verify_backfill_counts_as_todays_run() {
    let pool = common::pool().await;
    let vid: i64 = sqlx::query_scalar("INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("vfyv")).fetch_one(&pool).await.unwrap();
    let today = chrono::Local::now().date_naive();
    sqlx::query(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'backfill','scheduled','ok')")
        .bind(vid).execute(&pool).await.unwrap();
    assert!(getbloomdata_lib::scheduler::already_ran_today(&pool, vid, today)
        .await.unwrap(),
        "a completed scheduled verify must stop the EOD run from double-firing");
}
```

Run: `cargo test -p getbloomdata` and `cargo test --test quality -- --ignored` → PASS. `npm run check` → clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/scheduler.rs src-tauri/src/orchestrator.rs src-tauri/src/commands.rs src-tauri/tests/quality.rs src/lib/api.ts src/lib/SettingsScreen.svelte
git commit -m "feat: weekly verification re-fetch -- scheduled 5-weekday backfill catches restatements"
```

---

### Task 7: Migration 0008 — observation.currency

**Files:**
- Create: `src-tauri/migrations/0008_observation_currency.sql`
- Test: `src-tauri/tests/currency.rs` (new file, schema tests)

**Interfaces:**
- Produces: `observation.currency TEXT NULL`, backfilled from the current-belief `currency` attribute valid at each row's `obs_date`, and immutable via the extended `observation_append_only` trigger.

- [ ] **Step 1: Write the failing schema tests**

Create `src-tauri/tests/currency.rs`:

```rust
mod common;

use common::uniq;

#[tokio::test]
#[ignore = "requires postgres"]
async fn observation_currency_exists_and_is_append_only() {
    let pool = common::pool().await;
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq("CCY")).fetch_one(&pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(&pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'Last','numeric') RETURNING id")
        .bind(class).bind(uniq("CPX")).fetch_one(&pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("ccyv")).fetch_one(&pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(&pool).await.unwrap();
    let basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false")
        .fetch_one(&pool).await.unwrap();
    let oid: i64 = sqlx::query_scalar(
        "INSERT INTO observation (instrument_id, field_id, obs_date, layer,
                                  basis_id, value_num, run_id, currency)
         VALUES ($1,$2,'2026-08-13','raw',$3,101.5,$4,'GBp') RETURNING id")
        .bind(iid).bind(fid).bind(basis).bind(rid)
        .fetch_one(&pool).await.unwrap();
    let stored: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation WHERE id = $1")
        .bind(oid).fetch_one(&pool).await.unwrap();
    assert_eq!(stored.as_deref(), Some("GBp"), "verbatim, pence stay pence");
    let tampered = sqlx::query(
        "UPDATE observation SET currency = 'GBP' WHERE id = $1")
        .bind(oid).execute(&pool).await;
    assert!(tampered.is_err(),
            "currency is as immutable as the value it prices");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --test currency -- --ignored` → FAIL (column does not exist).

- [ ] **Step 3: Write the migration**

Create `src-tauri/migrations/0008_observation_currency.sql`:

```sql
-- P7/P2: currency becomes a dimension of the observation itself, not just a
-- resolution-time attribute. Stored VERBATIM from Bloomberg's CRNCY -- an
-- LSE line quoted in pence carries 'GBp', because raw storage records what
-- the number IS, never converts it. NULL for text values and for rows whose
-- instrument has no known currency.
ALTER TABLE observation ADD COLUMN currency TEXT;

-- Backfill existing numeric rows from the currently-believed currency
-- attribute valid at each row's own date.
UPDATE observation o SET currency = a.value
  FROM instrument_attr a
 WHERE a.instrument_id = o.instrument_id
   AND a.attr = 'currency'
   AND a.system_to = 'infinity'
   AND a.valid_from <= o.obs_date AND a.valid_to > o.obs_date
   AND o.value_num IS NOT NULL;

-- From here on, currency is as immutable as the value it prices: a
-- redenomination closes the row and inserts a new one (ingest raises
-- currency_changed when it does).
CREATE OR REPLACE FUNCTION observation_append_only() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value_num IS DISTINCT FROM OLD.value_num
     OR NEW.value_text IS DISTINCT FROM OLD.value_text
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.field_id <> OLD.field_id
     OR NEW.obs_date <> OLD.obs_date
     OR NEW.obs_time IS DISTINCT FROM OLD.obs_time
     OR NEW.granularity <> OLD.granularity
     OR NEW.layer <> OLD.layer
     OR NEW.basis_id IS DISTINCT FROM OLD.basis_id
     OR NEW.run_id <> OLD.run_id
     OR NEW.currency IS DISTINCT FROM OLD.currency THEN
    RAISE EXCEPTION
      'observations are append-only; close system_to and insert a corrected row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;
```

- [ ] **Step 4: Verify LF endings and run** — `git ls-files --eol` check as in Task 1, then `cargo test --test currency -- --ignored` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/migrations/0008_observation_currency.sql src-tauri/tests/currency.rs
git commit -m "feat(db): observation.currency -- stamped, backfilled, append-only (migration 0008)"
```

---

### Task 8: currency at ingest + in reads

**Files:**
- Modify: `src-tauri/src/ingest.rs` (stamping, comparison, `currency_changed`)
- Modify: `src-tauri/src/dataview.rs` (`ObsRow.currency`, query, CSV)
- Modify: `src/lib/api.ts` (`ObsRow.currency`), `src/lib/DataScreen.svelte` (Ccy column)
- Test: `src-tauri/tests/currency.rs` (append)

**Interfaces:**
- Produces: `ingest_outcome` stamps `observation.currency` from the instrument's current-belief `currency` attribute valid at the cell's `obs_date` (numeric cells only). An unchanged value whose currency changed is superseded with issue code `currency_changed` (severity `'warn'`). `dataview::ObsRow` gains `pub currency: Option<String>`; observations CSV header becomes `obs_date,value,currency,basis,run_id,recorded_at`.

- [ ] **Step 1: Write the failing tests**

Append to `src-tauri/tests/currency.rs`:

```rust
use chrono::NaiveDate;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::ingest;

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

async fn ccy_scaffold(pool: &sqlx::PgPool, stem: &str, ccy: &str) -> (i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(stem)).fetch_one(pool).await.unwrap();
    let iid: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1,'currency',$2,'2000-01-01','user')")
        .bind(iid).bind(ccy).execute(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,'Last','numeric') RETURNING id")
        .bind(class).bind(uniq("CPX")).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq("ccyr")).fetch_one(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (iid, fid, rid)
}

fn one_cell(iid: i64, fid: i64, v: f64) -> FetchOutcome {
    FetchOutcome {
        cells: vec![ObsCell { instrument_id: iid, field_id: fid,
                              obs_date: d("2026-08-13"),
                              value: CellValue::Num(v) }],
        problems: vec![],
    }
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn ingest_stamps_the_instruments_currency_verbatim() {
    let pool = common::pool().await;
    let (iid, fid, rid) = ccy_scaffold(&pool, "CST", "GBp").await;
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, 4321.0)).await.unwrap();
    let ccy: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation
          WHERE instrument_id = $1 AND field_id = $2 AND system_to = 'infinity'")
        .bind(iid).bind(fid).fetch_one(&pool).await.unwrap();
    assert_eq!(ccy.as_deref(), Some("GBp"), "pence recorded as pence");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_currency_change_supersedes_and_raises_currency_changed() {
    let pool = common::pool().await;
    let (iid, fid, rid) = ccy_scaffold(&pool, "CCH", "EUR").await;
    ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, 100.0)).await.unwrap();
    // Redenomination: same value, the believed currency moves EUR -> USD.
    sqlx::query(
        "UPDATE instrument_attr SET system_to = now()
          WHERE instrument_id = $1 AND attr = 'currency' AND system_to = 'infinity'")
        .bind(iid).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1,'currency','USD','2000-01-01','user')")
        .bind(iid).execute(&pool).await.unwrap();
    let s = ingest::ingest_outcome(&pool, rid, &one_cell(iid, fid, 100.0)).await.unwrap();
    assert_eq!(s.superseded, 1, "same number, different unit: NOT unchanged");
    let code: String = sqlx::query_scalar(
        "SELECT code FROM ingest_issue
          WHERE run_id = $1 AND instrument_id = $2 AND code = 'currency_changed'")
        .bind(rid).bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(code, "currency_changed");
    let current: Option<String> = sqlx::query_scalar(
        "SELECT currency FROM observation
          WHERE instrument_id = $1 AND field_id = $2 AND system_to = 'infinity'")
        .bind(iid).bind(fid).fetch_one(&pool).await.unwrap();
    assert_eq!(current.as_deref(), Some("USD"));
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --test currency -- --ignored` → first new test FAILS (currency NULL — nothing stamps it yet).

- [ ] **Step 3: Implement ingest stamping**

In `src-tauri/src/ingest.rs`, inside `ingest_outcome` after the `raw_basis` query, load the currency periods once:

```rust
    // The instrument's believed currency, per validity period, loaded once --
    // stamped on every numeric cell so the observation carries its unit.
    let ids: Vec<i64> = {
        let mut v: Vec<i64> = outcome.cells.iter().map(|c| c.instrument_id).collect();
        v.sort_unstable(); v.dedup(); v
    };
    let ccy_periods: Vec<(i64, String, chrono::NaiveDate, chrono::NaiveDate)> =
        if ids.is_empty() { Vec::new() } else {
            sqlx::query_as(
                "SELECT instrument_id, value, valid_from, valid_to
                   FROM instrument_attr
                  WHERE attr = 'currency' AND system_to = 'infinity'
                    AND instrument_id = ANY($1)")
                .bind(&ids).fetch_all(pool).await?
        };
    let currency_at = |iid: i64, d: chrono::NaiveDate| -> Option<&str> {
        ccy_periods.iter()
            .find(|(i, _, from, to)| *i == iid && *from <= d && *to > d)
            .map(|(_, v, _, _)| v.as_str())
    };
```

In the per-cell loop:
- compute `let ccy = num.is_some().then(|| currency_at(c.instrument_id, c.obs_date)).flatten();`
- the current-row SELECT gains `currency`: `SELECT id, value_num, value_text, currency FROM observation …` with tuple type `Option<(i64, Option<f64>, Option<String>, Option<String>)>`;
- the unchanged test becomes: values equal AND `old_ccy.as_deref() == ccy`;
- when values are equal but the currency differs, supersede and write a `currency_changed` issue instead of `value_superseded`:
```rust
        if let Some((id, old_num, old_text, old_ccy)) = current {
            let same_value = old_num == num && old_text == text;
            if same_value && old_ccy.as_deref() == ccy {
                unchanged += 1;
                continue;
            }
            sqlx::query("UPDATE observation SET system_to = now() WHERE id = $1")
                .bind(id).execute(&mut *tx).await?;
            let (code, detail) = if same_value {
                ("currency_changed", format!(
                    "currency changed {} -> {} with the value unchanged -- \
                     redenomination or master-data correction",
                    old_ccy.as_deref().unwrap_or("(none)"),
                    ccy.unwrap_or("(none)")))
            } else {
                ("value_superseded", format!("stored value {} superseded by {}",
                    describe(&old_num, &old_text), describe(&num, &text)))
            };
            sqlx::query(
                "INSERT INTO ingest_issue
                   (run_id, instrument_id, field_id, obs_date, severity, code, detail)
                 VALUES ($1,$2,$3,$4,'warn',$5,$6)")
                .bind(run_id).bind(c.instrument_id).bind(c.field_id).bind(c.obs_date)
                .bind(code).bind(&detail)
                .execute(&mut *tx).await?;
            superseded += 1;
        }
```
(hoist the `describe` closure from Task 5 above the loop so both arms share it);
- the INSERT gains the column: `INSERT INTO observation (instrument_id, field_id, obs_date, granularity, layer, basis_id, value_num, value_text, run_id, currency) VALUES ($1,$2,$3,'eod','raw',$4,$5,$6,$7,$8)` with `.bind(ccy)` appended.

- [ ] **Step 4: Expose in reads**

`src-tauri/src/dataview.rs`: `ObsRow` gains `pub currency: Option<String>,` (after `value_text`); the `observations` SELECT adds `o.currency,`; `export_observations_csv` header becomes `"obs_date,value,currency,basis,run_id,recorded_at\n"` and the row line inserts `r.currency.clone().unwrap_or_default(),` after `value`.
`src/lib/api.ts`: `ObsRow` gains `currency: string | null;`.
`src/lib/DataScreen.svelte`: raw observations table header (`:245`) gains `<th>Ccy</th>` after `<th>Value</th>`; each row gains `<td class="thin">{o.currency ?? "—"}</td>` after the value cell.

- [ ] **Step 5: Run tests** — `cargo test --test currency -- --ignored` and `cargo test --test quality -- --ignored` (Task 5's supersession test must still pass) → PASS. `npm run check` → clean.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/ingest.rs src-tauri/src/dataview.rs src-tauri/tests/currency.rs src/lib/api.ts src/lib/DataScreen.svelte
git commit -m "feat: every numeric observation carries its currency; redenominations supersede and alert"
```

---

### Task 9: cross-currency stitch guard

**Files:**
- Modify: `src-tauri/src/stitch.rs` (`stitched_series`)
- Test: `src-tauri/tests/currency.rs` (append)

**Interfaces:**
- Produces: `stitched_series` stops (with a `stopped` message containing `"cross-currency"`) before splicing a predecessor whose current-belief currency differs from the queried instrument's. Volume series exempt. Unknown currencies (either side `None`) proceed as before — refusing on ignorance would break every user-created instrument.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/currency.rs`:

```rust
use getbloomdata_lib::adjust::AdjustMode;
use getbloomdata_lib::stitch;

/// Two instruments with one observation each and a confirmed merger link.
async fn linked_pair(pool: &sqlx::PgPool, stem: &str,
                     pred_ccy: &str, succ_ccy: &str) -> (i64, i64, i64) {
    let (pred, fid, rid) = ccy_scaffold(pool, stem, pred_ccy).await;
    let succ: i64 = sqlx::query_scalar(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap();
    sqlx::query(
        "INSERT INTO instrument_attr (instrument_id, attr, value, valid_from, source)
         VALUES ($1,'currency',$2,'2000-01-01','user')")
        .bind(succ).bind(succ_ccy).execute(pool).await.unwrap();
    // predecessor priced before the junction, successor at/after it
    let basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false")
        .fetch_one(pool).await.unwrap();
    for (iid, date, px) in [(pred, "2026-06-30", 50.0), (succ, "2026-07-01", 100.0)] {
        sqlx::query(
            "INSERT INTO observation (instrument_id, field_id, obs_date, layer,
                                      basis_id, value_num, run_id)
             VALUES ($1,$2,$3::date,'raw',$4,$5,$6)")
            .bind(iid).bind(fid).bind(date).bind(basis).bind(px).bind(rid)
            .execute(pool).await.unwrap();
    }
    sqlx::query(
        "INSERT INTO instrument_link (predecessor_id, successor_id, link_type,
                                      effective_date, evidence, exchange_ratio,
                                      confirmed_by, confirmed_at)
         VALUES ($1,$2,'merger','2026-07-01','{}'::jsonb,2.0,'test',now())")
        .bind(pred).bind(succ).execute(pool).await.unwrap();
    (pred, succ, fid)
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn stitching_refuses_a_cross_currency_junction() {
    let pool = common::pool().await;
    let (_pred, succ, fid) = linked_pair(&pool, "XCCY", "EUR", "USD").await;
    let s = stitch::stitched_series(&pool, succ, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.as_deref().unwrap_or("").contains("cross-currency"),
            "stopped: {:?}", s.stopped);
    assert!(s.rows.iter().all(|r| r.source_instrument_id == succ),
            "no predecessor rows may be spliced in a foreign currency");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn stitching_still_works_when_currencies_match() {
    let pool = common::pool().await;
    let (pred, succ, fid) = linked_pair(&pool, "SCCY", "USD", "USD").await;
    let s = stitch::stitched_series(&pool, succ, fid, AdjustMode::Raw, 100)
        .await.unwrap();
    assert!(s.stopped.is_none(), "stopped: {:?}", s.stopped);
    assert!(s.rows.iter().any(|r| r.source_instrument_id == pred),
            "the predecessor segment must be spliced");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --test currency stitching -- --ignored` → the cross-currency test FAILS (no guard yet; the same-currency one should already pass and pins the non-regression).

- [ ] **Step 3: Implement**

In `src-tauri/src/stitch.rs`, add near `segment_label`:

```rust
/// The instrument's currently-believed currency, valid today. None for a
/// user-created instrument Bloomberg was never asked about -- the guard
/// below deliberately does not refuse on ignorance.
async fn current_currency(pool: &sqlx::PgPool, instrument_id: i64)
    -> crate::error::AppResult<Option<String>>
{
    Ok(sqlx::query_scalar(
        "SELECT value FROM instrument_attr
          WHERE instrument_id = $1 AND attr = 'currency'
            AND system_to = 'infinity'
            AND valid_from <= CURRENT_DATE AND valid_to > CURRENT_DATE
          ORDER BY valid_from DESC LIMIT 1")
        .bind(instrument_id).fetch_optional(pool).await?)
}
```

In `stitched_series`, before the junction loop:

```rust
    // P7: a share ratio converts share COUNTS, not currencies. Splicing a
    // EUR history onto a USD series with only a ratio fabricates numbers, so
    // a junction whose two sides carry different believed currencies stops
    // the walk. GBp vs GBP counts: pence are not pounds. Volumes are exempt
    // (a share count has no currency).
    let target_ccy = if is_volume { None }
                     else { current_currency(pool, instrument_id).await? };
```

Inside the loop, immediately after `let pred = crate::adjust::adjusted_series(...)` (before the ratio computation):

```rust
        if let Some(t) = target_ccy.as_deref() {
            if let Some(p) = current_currency(pool, j.predecessor_id).await? {
                if p != t {
                    stopped = Some(format!(
                        "cross-currency link at {d}: predecessor quoted in {p}, \
                         this instrument in {t}; extension refused -- no FX \
                         conversion exists"));
                    break;
                }
            }
        }
```

- [ ] **Step 4: Run tests** — `cargo test --test currency -- --ignored` → PASS both; `cargo test --test stitch -- --ignored` → existing stitch suite unaffected (its fixtures set no currency attrs → guard is inert).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/stitch.rs src-tauri/tests/currency.rs
git commit -m "feat: stitching refuses cross-currency junctions -- a ratio converts shares, not euros"
```

---

### Task 10: full verification + docs

**Files:**
- Modify: `docs/superpowers/plans/2026-08-20-p7-quality-gate-and-currency.md` (tick executed checkboxes)
- No production code.

- [ ] **Step 1: Full test sweep**

From `src-tauri/`: `cargo test` (all pure suites), then `cargo test -- --ignored` (full DB suite — requires local Postgres; if unavailable, say so explicitly in the final report rather than claiming green). From repo root: `npm run check`.

- [ ] **Step 2: Manual smoke notes (needs the GUI + Terminal — record, do not fake)**

Add to this plan file a "Live smoke" section listing the three checks that need a Bloomberg Terminal session, unchecked: (1) a real run produces `quality` issues when a threshold is deliberately set low; (2) the Friday verify run appears as kind=backfill/trigger=scheduled and `value_superseded` fires on a genuinely restated close; (3) an LSE instrument's observations show `GBp`.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/plans/2026-08-20-p7-quality-gate-and-currency.md
git commit -m "docs: P7 quality gate and currency dimension shipped"
```

---

## Self-Review

- **Spec coverage:** quality gate (Tasks 1-4), supersession alerts (Task 5), verify re-fetch (Task 6), currency stamping + reads (Tasks 7-8), stitch guard (Task 9). The assessment's "close the PX_LAST-vs-Terminal smoke check" needs a live Terminal and is recorded as an explicit unchecked item (Task 10 Step 2), not silently dropped.
- **Type consistency:** `QcConfig`/`SeriesFinding`/`run_quality_gate` names match between Tasks 3-4; `quality_findings` field name matches across orchestrator/scheduler/api.ts/RunScreen; `verify_dow: Option<i16>` matches schema SMALLINT across commands/api.ts; `ObsRow.currency: Option<String>` matches `string | null`.
- **Known compile ripple:** adding `quality_findings` to `RunOutcome::Completed` breaks existing destructuring patterns; Task 4 Step 4.3 instructs finding and fixing all of them, tests included.
- **CellProblem construction in Task 3's test** is flagged for field-type verification against the real `fetch.rs` definition before writing.
