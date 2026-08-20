# P5 Merger Stitching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend a surviving instrument's series backward through confirmed merger/rename links, spliced in successor units, derived on read — the last founding requirement (coherent timeseries across fund merges).

**Architecture:** `src-tauri/src/stitch.rs`: a pure `plan_chain` over confirmed link rows + `stitched_series` composing P4's `adjusted_series` per segment with junction splice ratios. One command + CSV export; Data tab checkbox + Source column.

**Tech Stack:** unchanged. No migration (the ratio is derived, not stored).

**Spec:** `docs/superpowers/specs/2026-08-21-p5-merger-stitching-design.md`.

## Global Constraints

- Only links with `confirmed_by IS NOT NULL` are followed; spinoffs never.
- rename/share_class_change → ratio 1; merger/conversion → derived ratio (successor value at/after D ÷ predecessor value before D, both mode-adjusted); underivable ratio stops the walk with a reported reason.
- Effective dates must strictly descend along the walk; ties or in-range dates stop it (reported).
- Volume-kind fields concatenate unscaled (reported in the segment list).
- Depth cap 10, cycle guard. No writes, no Bloomberg calls.

---

### Task 1: `stitch.rs` — pure chain planner + unit tests

**Files:** Create `src-tauri/src/stitch.rs`; modify `src-tauri/src/lib.rs`.

**Interfaces (produced):**
```rust
#[derive(Debug, Clone)]
pub struct LinkRow { pub predecessor_id: i64, pub successor_id: i64,
                     pub link_type: String, pub effective_date: NaiveDate }
#[derive(Debug, PartialEq)]
pub struct Junction { pub predecessor_id: i64, pub effective_date: NaiveDate,
                      pub link_type: String }
#[derive(Debug, PartialEq)]
pub enum ChainStop { End, Ambiguous(NaiveDate), Cycle, DepthCap }
pub fn plan_chain(target: i64, links: &[LinkRow]) -> (Vec<Junction>, ChainStop);
```
`plan_chain`: repeatedly find confirmed non-spinoff links whose `successor_id` is the current instrument and whose `effective_date` is strictly before the previous junction's date (first step: unbounded); pick the latest; tie → `Ambiguous`; seen-set → `Cycle`; 10 junctions → `DepthCap`.

- [x] Unit tests first: straight chain A→B→C queried at C yields junctions [B@d2, A@d1] with d2 > d1; spinoff links ignored; tie on effective_date → `Ambiguous`; cycle A→B, B→A → `Cycle`; link dated inside an already-covered range is skipped as candidate (dates must descend).
- [x] Implement; `cargo test --lib stitch` green. Commit `feat: P5 chain planner over confirmed links`.

### Task 2: `stitched_series` — DB composition + integration tests

**Files:** `src-tauri/src/stitch.rs`, `src-tauri/tests/stitch.rs` (new).

**Interfaces (produced):**
```rust
#[derive(Debug, serde::Serialize)]
pub struct StitchRow { pub obs_date: NaiveDate, pub value: f64,
                       pub source_instrument_id: i64 }
#[derive(Debug, serde::Serialize)]
pub struct SegmentInfo { pub instrument_id: i64, pub label: Option<String>,
                         pub from: Option<NaiveDate>, pub to: Option<NaiveDate>,
                         pub link_type: Option<String>, pub ratio: Option<f64>,
                         pub note: Option<String> }
#[derive(Debug, serde::Serialize)]
pub struct StitchedSeries { pub rows: Vec<StitchRow>,      // obs_date DESC
                            pub segments: Vec<SegmentInfo>,
                            pub stopped: Option<String> }  // ambiguity/ratio reason
pub async fn stitched_series(pool, instrument_id, field_id, mode: AdjustMode,
                             limit: i64) -> AppResult<StitchedSeries>;
pub async fn has_confirmed_predecessors(pool, instrument_id) -> AppResult<bool>;
```
Loads confirmed links touching the walk (`WHERE successor_id = ANY(...)` iteratively is fine at depth ≤10), plans the chain, then per segment calls `adjust::adjusted_series` (limit 5000) on that instrument, truncates to the segment's date window, derives the junction ratio from the adjusted values (predecessor field: same field_id — the field belongs to the shared asset class), multiplies the cumulative ratio into older segments (volume kind: ratio 1 + note "volumes concatenated unscaled"). Total rows clamped to `limit` after concatenation.

- [x] Integration tests (scaffold like tests/adjust.rs, two instruments in one class):
  1. `a_confirmed_merger_extends_the_survivor_backward` — A has obs 100.0@2026-01-05 (its last), B has 25.0@2026-01-12 onward; confirmed merger link A→B effective 2026-01-12; query B → A's row appears as 25.0 (ratio 0.25 applied), `segments[1].ratio == Some(0.25)`, source ids correct.
  2. `an_unconfirmed_link_is_never_followed` — same setup, link NOT confirmed → only B's rows, one segment.
  3. `a_rename_splices_at_ratio_one` — link_type 'rename', junction values differ slightly → predecessor values UNCHANGED, ratio Some(1.0).
  4. `a_missing_junction_observation_stops_the_walk_with_a_reason` — predecessor has no obs before D → only B's segment, `stopped` mentions the ratio.
- [x] Suites green. Commit `feat: stitched series across confirmed merger links`.

### Task 3: command + CSV export

**Files:** `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`, `src-tauri/src/stitch.rs`, `src-tauri/tests/stitch.rs`.

- [x] `list_stitched(instrument_id, field_id, mode: String, limit) -> StitchedSeries`; `has_confirmed_predecessors(instrument_id) -> bool`; `export_stitched_csv(instrument_id, field_id, mode, path) -> u64` header `obs_date,value,source_instrument_id` (full depth 5000). Register all three.
- [x] Integration test: CSV line count and the spliced value on the predecessor row.
- [x] Suites green. Commit `feat: stitched-series command and CSV export`.

### Task 4: Data tab UI

**Files:** `src/lib/api.ts`, `src/lib/DataScreen.svelte`.

- [x] `api.ts`: `StitchRow`/`SegmentInfo`/`StitchedSeries` types; `listStitched`, `hasConfirmedPredecessors`, `exportStitchedCsv`.
- [x] DataScreen: when the selected instrument `hasConfirmedPredecessors`, show a checkbox `Extend through confirmed mergers`; when checked, the observations area renders the stitched table (Date / Value / Source label or id on foreign rows), a thin segment-report line (`B 2026-01-12→…; A ×0.25 until 2026-01-11`), the `stopped` reason as a hint when present; CSV path seeds `stitched_{iid}_{fid}_{mode}.csv` and exports via the new command. Works combined with the Series selector (mode passes through).
- [x] `svelte-check` 0 errors. Commit `feat(ui): merger-stitched series in the Data tab`.

### Task 5: Verification + docs + merge

- [x] Full suites (unit + `--ignored` minus live smoke) green; svelte-check clean.
- [x] Spec status → IMPLEMENTED; memory updated; plan ticked.
- [x] Fast-forward `master`; relaunch the app.
- [x] Commit `docs: P5 merger stitching shipped`.
