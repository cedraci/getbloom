# P4 Adjustment Engine Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Derive split-adjusted and net (split + dividend) price/volume series on read from stored RAW observations + the corp_action factor chain, shown in the Data tab and exportable as CSV. Nothing stored, ever.

**Architecture:** New `src-tauri/src/adjust.rs`: a pure `apply_chain` over `(date, value)` pairs and factor events (unit-testable without a DB), plus `adjusted_series` that loads current raw observations and current factors. One read command + one CSV export command. Data tab gains a Series selector.

**Tech Stack:** unchanged.

**Spec:** `docs/superpowers/specs/2026-08-21-p4-adjustment-engine-design.md`.

## Global Constraints

- No writes to `observation` or anywhere else — derivation only.
- Operator semantics (P0 §10.1): prices op1 `/f`, op2 `*f`, op3 `+f`; volumes the OPPOSITE (`*f`, `/f`, `-f`). Volume = field mnemonic contains `VOLUME` (upper-cased).
- Volumes only receive flag-3 events in EVERY mode (flag 1 = prices only).
- Events apply in chronological order for `event_date > obs_date`.
- Unusable factor rows (any of event_date/amount/operator/flag null) are excluded and counted, never silently skipped.
- No Bloomberg calls anywhere in P4.

---

### Task 1: `adjust.rs` — pure engine + unit tests

**Files:** Create `src-tauri/src/adjust.rs`; modify `src-tauri/src/lib.rs` (module).

**Interfaces (produced):**
```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum AdjustMode { Raw, Splits, All }
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeriesKind { Price, Volume }
pub struct FactorEvent { pub event_date: NaiveDate, pub amount: f64,
                         pub operator: i16, pub flag: i16 }
pub fn series_kind(mnemonic: &str) -> SeriesKind; // contains "VOLUME" => Volume
pub fn apply_chain(kind: SeriesKind, mode: AdjustMode,
                   obs_date: NaiveDate, raw: f64, events: &[FactorEvent]) -> f64;
// events MUST be sorted ascending by event_date by the caller.
```
`apply_chain`: for each event with `event_date > obs_date`, skip unless mode admits it (Splits: flag == 3; All: any; volumes additionally require flag == 3), then apply the operator per kind. Raw mode returns `raw` untouched.

- [x] Unit tests first (in-module), then implement:
  1. `aapl_split_divides_prices_and_multiplies_volumes` — event (2020-08-31, 4.0, op 1, flag 3): price 400.0 on 2020-08-28 → 100.0 (Splits and All); volume 1000.0 → 4000.0; obs ON 2020-08-31 unchanged (strictly `>`).
  2. `dividend_factors_touch_prices_only_and_only_in_all_mode` — event (2025-05-05, 0.994902, op 2, flag 1): price before → `*0.994902` in All, untouched in Splits; volume untouched in All.
  3. `twin_same_day_events_both_apply` — two op-2 flag-1 events same date: product of both factors.
  4. `additive_operators_apply_in_chronological_order` — events e1 (2020-01-10, 2.0, op 3, flag 3) and e2 (2020-06-10, 3.0, op 2, flag 3) on price 10.0 dated 2019-12-31: `(10+2)*3 = 36`, NOT `10*3+2 = 32`.
  5. `raw_mode_is_identity` and `series_kind` detection (`PX_VOLUME` → Volume, `PX_LAST` → Price).
- [x] `cargo test --lib adjust` green. Commit `feat: P4 pure adjustment engine over the factor chain`.

### Task 2: `adjusted_series` — DB loader + integration tests

**Files:** `src-tauri/src/adjust.rs`, `src-tauri/tests/adjust.rs` (new, pipeline-style scaffold).

**Interfaces (produced):**
```rust
#[derive(Debug, serde::Serialize)]
pub struct AdjRow { pub obs_date: NaiveDate, pub raw: f64, pub adjusted: f64 }
#[derive(Debug, serde::Serialize)]
pub struct AdjSeries { pub rows: Vec<AdjRow>,           // obs_date DESC
                       pub factors_used: usize, pub unusable_factors: usize }
pub async fn adjusted_series(pool, instrument_id, field_id, mode: AdjustMode,
                             limit: i64) -> AppResult<AdjSeries>;
```
Loads: current numeric observations (`layer='raw'`, `system_to='infinity'`, ORDER BY obs_date DESC, LIMIT clamp 1..=5000, value_num NOT NULL); the field's mnemonic (for `series_kind`); current `EQY_DVD_ADJUST_FACT` rows — fully typed ones become `FactorEvent`s sorted ascending, rows with any null typed column count into `unusable_factors`. `factors_used` = events admitted by the mode/kind filter (union over the series, i.e. events applied to at least the oldest row).

- [x] Integration tests: seed two observations (one before, one after a stored split factor row) + one unparsed factor row (nulls, JSON fallback key); `adjusted_series(All)` → older row adjusted, newer untouched, `unusable_factors == 1`; `Raw` mode → adjusted == raw everywhere.
- [x] Suites green. Commit `feat: adjusted series derived from stored observations and factors`.

### Task 3: commands + CSV export

**Files:** `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/dataview.rs` (reuse `csv_field`/`csv_line`), `src-tauri/tests/adjust.rs`.

- [x] `list_adjusted(instrument_id, field_id, mode: String, limit) -> AdjSeries` (mode parsed: "raw"|"splits"|"all", Validation error otherwise); `export_adjusted_csv(instrument_id, field_id, mode, path) -> u64` writing header `obs_date,raw,adjusted` (current rows, full series at limit 5000). Register both.
- [x] Integration test: export writes rows+1 lines with adjusted values matching `adjusted_series`.
- [x] Suites green. Commit `feat: adjusted-series command and CSV export`.

### Task 4: Data tab UI — Series selector

**Files:** `src/lib/api.ts`, `src/lib/DataScreen.svelte`.

- [x] `api.ts`: `AdjSeries`/`AdjRow` types; `listAdjusted(instrumentId, fieldId, mode, limit)`; `exportAdjustedCsv(...)`.
- [x] DataScreen: a `Series` select (`raw` "Raw (as stored)" / `splits` "Split-adjusted" / `all` "Split + dividend (net)"). Raw keeps today's table (with supersession toggle). Non-raw loads `listAdjusted` and renders Date / Raw / Adjusted / (factors used, unusable warning line); superseded toggle disabled (derived series has no system time); CSV export path seeds `adj_{iid}_{fid}_{mode}.csv` and calls the new exporter. A thin note: "Derived on read from the stored factor chain — nothing is stored."
- [x] `svelte-check` 0 errors (1 pre-existing warning allowed). Commit `feat(ui): raw / split-adjusted / net series in the Data tab`.

### Task 5: Verification + docs + merge

- [x] Full suites (unit + `--ignored` minus live smoke) green.
- [x] P4 spec status → IMPLEMENTED; roadmap note in security-master design §11 if present; memory updated.
- [x] Fast-forward `master` (includes the .gitattributes fix commit).
- [x] Commit `docs: P4 adjustment engine shipped`.
