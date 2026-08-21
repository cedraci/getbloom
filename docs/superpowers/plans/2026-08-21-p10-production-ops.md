# P10: Production Operations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run the feed like production — CI for the whole test suite, an honest hit ledger with visible usage, automatic gap backfill after machine downtime, explicit non-trading-day evidence, a `current_eod` SQL view for downstream consumers, and connection settings in the UI.

**Architecture:** CI ships first so the rest of the wave lands under it. The ledger fix moves run-hit recording from the pre-flight estimate to a pure `dispatched_hits` over the planned requests (corp actions keep charging at the wire seam — each hit lands exactly once). Gap backfill is an orchestrator entry point the scheduler calls before the day's main run, gated by the existing budget levels. Non-trading evidence comes from the sidecar switching to NIL-fill and emitting `no_data` problems — Rust ingest Rule A already consumes those.

**Tech Stack:** Rust (sqlx/Postgres, Tauri 2), Svelte 5, Python (BLPAPI sidecar), GitHub Actions.

**Spec:** `docs/superpowers/specs/2026-08-21-p9-p10-multi-asset-and-production-ops-design.md` (P10 half). Read it first.

## Global Constraints

- **NO hard budget cap** — standing user decision 2026-08-20. The soft limit warns; `HardConfirm` gates at 2× soft; the scheduler never auto-confirms past `Ok`.
- `run` and `hit_ledger` rows are never rewritten or deleted.
- Migration files MUST be LF-only (verify `git ls-files --eol src-tauri/migrations`); after adding one, `touch src-tauri/tests/common/mod.rs`.
- DB integration tests: `#[ignore = "requires postgres"]`, shared `bloom_test` via `tests/common/mod.rs::pool()`, `common::uniq()` for every UNIQUE-constrained value. Commands: `cargo test` (pure), `cargo test --no-fail-fast -- --ignored` (from `src-tauri/`). Known permanent bloom_test failure: `smoke_real_bloomberg_end_to_end`. Frontend: `npm run check`, 0 errors.
- The Python sidecar (`src-tauri/scripts/blp_fetch.py`) is P0-measured against the real wire: never change response parsing without a canned-response test in `src-tauri/scripts/test_blp_fetch.py`.
- Every commit message ends with: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- House style: `//!` module docs explain WHY; advisory sub-steps log-and-continue.

## File Structure

- Create: `.github/workflows/ci.yml`
- Create: `src-tauri/migrations/0013_current_eod_view.sql`
- Create: `src-tauri/tests/ops.rs` (ledger, gap backfill, current_eod, payload serialization)
- Modify: `src-tauri/src/fetch.rs` (Override + dispatched_hits + SidecarPayload host/port), `budget.rs` (pub weekdays_between), `orchestrator.rs` (record dispatched; run_gap_backfill), `scheduler.rs` (tick calls gap backfill), `commands.rs` + `lib.rs` (budget_today, AppConfig fields, startup URL precedence), `master_fetch.rs` (payload host/port)
- Modify: `src-tauri/scripts/blp_fetch.py` (NIL fill + problem emission), `src-tauri/scripts/test_blp_fetch.py`
- Modify: `src/lib/api.ts`, `src/lib/RunScreen.svelte` (hits-today line), `src/lib/SettingsScreen.svelte` (connection fields)

---

### Task 1: CI workflow

**Files:**
- Create: `.github/workflows/ci.yml`

**Interfaces:**
- Produces: on every push/PR to master, two jobs — `rust` (unit + full ignored Postgres suite against a service container) and `frontend` (`npm run check`). The only Terminal-dependent test, `smoke_real_bloomberg_end_to_end`, is skipped by name.

- [ ] **Step 1: Write the workflow**

```yaml
name: CI
on:
  push:
    branches: [master]
  pull_request:

jobs:
  rust:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:17
        env:
          POSTGRES_PASSWORD: postgres
        ports: ['5432:5432']
        options: >-
          --health-cmd pg_isready --health-interval 10s
          --health-timeout 5s --health-retries 5
    steps:
      - uses: actions/checkout@v4
      - name: Tauri system dependencies
        run: |
          sudo apt-get update
          sudo apt-get install -y libwebkit2gtk-4.1-dev libgtk-3-dev \
            libayatana-appindicator3-dev librsvg2-dev build-essential libssl-dev
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: src-tauri
      - name: Unit tests
        working-directory: src-tauri
        run: cargo test --no-fail-fast
      - name: Create test database
        run: PGPASSWORD=postgres createdb -h localhost -U postgres bloom_test
      - name: Postgres integration tests
        working-directory: src-tauri
        env:
          BLOOM_TEST_DATABASE_URL: postgres://postgres:postgres@localhost:5432/bloom_test
        run: cargo test --no-fail-fast -- --ignored --skip smoke_real_bloomberg

  frontend:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: 20
      - run: npm ci
      - run: npm run check
```

Note: `tests/db_integration.rs::test_url()` has no URL fallback — the env var above is mandatory, which is exactly what we want in CI.

- [ ] **Step 2: Verify the RED→GREEN loop on the real runner** (a workflow cannot be watched fail locally — its first run IS the test): commit to a branch, push, and watch:

```bash
git checkout -b p10-production-ops
git add .github/workflows/ci.yml
git commit -m "ci: unit + Postgres integration + svelte-check on every push"
git push -u origin p10-production-ops
gh run watch
```

Expected first-run failures to fix in follow-up commits until green: missing apt package for the Tauri build, `npm ci` requiring `package-lock.json` (if absent, commit one via `npm install --package-lock-only`), Postgres auth. Iterate on this branch until both jobs pass; each fix is its own commit.

- [ ] **Step 3: Confirm green** — `gh run view --log-failed` shows nothing; both jobs green. The branch stays open — the rest of P10 lands on it under CI.

---

### Task 2: Honest hit ledger — record dispatched hits, end the corp-action double-count

**Files:**
- Modify: `src-tauri/src/fetch.rs` (new `Override` struct + `dispatched_hits`), `src-tauri/src/budget.rs` (make `weekdays_between` pub), `src-tauri/src/orchestrator.rs` (`execute` at :247)
- Test: inline unit tests in `fetch.rs`; integration in new `src-tauri/tests/ops.rs`

**Interfaces:**
- Consumes: `fetch::plan_requests(req) -> AppResult<Vec<RequestSpec>>` (pure, fetch.rs:177); `budget::record_hits(pool, run_id, n)` (budget.rs:54); `budget::weekdays_between(start, end)` (budget.rs:12 — becomes `pub`).
- Produces: `pub fn dispatched_hits(specs: &[RequestSpec], start: NaiveDate, end: NaiveDate) -> i64` — Σ per spec of `securities.len() × fields.len() × (weekdays_between(start, end) for kind "history"; 1 for "reference")`. `orchestrator::execute` records **this** into `hit_ledger` (still unconditionally, even on fetch failure — Bloomberg was asked); `run.estimated_hits` keeps the pre-flight gate estimate. Corp actions keep charging at the wire seam only (master_fetch.rs:456) — each hit lands in the ledger exactly once.

- [ ] **Step 1: Write the failing unit test** (inline `#[cfg(test)]` in fetch.rs, next to the existing plan_requests tests):

```rust
#[test]
fn dispatched_hits_counts_only_planned_requests() {
    // 2 assets, 1 numeric field, 1 text field, 3-weekday range
    // (2026-08-17 Mon .. 2026-08-19 Wed). plan_requests drops the text
    // field on multi-day ranges, so dispatched = 2 secs x 1 field x 3 days = 6,
    // while the naive estimate (estimate_backfill_hits) would say
    // 2 x 2 x 3 = 12.
    let req = /* build FetchRequest with the fixture above -- mirror the
                 neighbouring plan_requests test fixtures in this file */;
    let specs = plan_requests(&req).unwrap();
    assert_eq!(dispatched_hits(&specs, req.start, req.end), 6);
}
```

(Fixture assembly mirrors the existing `plan_requests` unit tests in the same file — copy one and add the text field; the assertion values above are the contract.)

- [ ] **Step 2: Run to verify failure** — `cargo test dispatched_hits` (from `src-tauri/`). Expected: compile error, `dispatched_hits` not found.

- [ ] **Step 3: Implement** in fetch.rs:

```rust
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
```

Make `budget::weekdays_between` `pub` (it is crate-internal today; check its exact name/signature at budget.rs:12 and keep it). In `orchestrator::execute` (:247), replace `budget::record_hits(pool, run_id, estimated)` with:

```rust
// Ledger gets what was dispatched; run.estimated_hits keeps the gate number.
let dispatched = fetch::plan_requests(req).map(|s| fetch::dispatched_hits(&s, start, end))
    .unwrap_or(estimated);
if let Err(e) = budget::record_hits(pool, run_id, dispatched).await {
    eprintln!("hit ledger write failed for run {run_id}: {e}");
}
```

(keeping the existing error-handling shape at :247 — read it first; if it currently `?`s, keep `?`).

- [ ] **Step 4: Write the failing integration test** — create `src-tauri/tests/ops.rs`:

```rust
//! P10 production-ops behaviours: honest ledger, gap backfill, current_eod.
mod common;
use common::uniq;
use chrono::NaiveDate;
fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

#[tokio::test]
#[ignore = "requires postgres"]
async fn the_ledger_records_dispatched_hits_not_the_gate_estimate() {
    let pool = common::pool().await;
    // Scaffold one instrument + 1 numeric field view (mirror the EmptyFetcher
    // pattern in tests/quality.rs -- a DataFetcher returning an empty outcome),
    // then run_eod_with for one day.
    // 1 security x 1 field x 1 day = 1 dispatched hit; assert the run's
    // hit_ledger row says exactly 1 (not 1 + corp-action estimate).
    let hits: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits), 0)::bigint FROM hit_ledger WHERE run_id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(hits, 1);
}
```

(Scaffold assembly mirrors `tests/quality.rs`'s `EmptyFetcher` + view fixture — read that file and reuse; the assertion is the contract.)

- [ ] **Step 5: Run all** — `cargo test dispatched_hits` → PASS; `cargo test --no-fail-fast --test ops --test pipeline --test quality -- --ignored` → PASS (if an existing pipeline test pinned the old `estimated`-goes-to-ledger behaviour, update that test — the spec explicitly changes the contract, note it in the commit message).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/fetch.rs src-tauri/src/budget.rs src-tauri/src/orchestrator.rs src-tauri/tests/ops.rs
git commit -m "fix: hit ledger records dispatched hits -- corp actions no longer double-counted"
```

---

### Task 3: Budget visibility — `budget_today` command + RunScreen line

**Files:**
- Modify: `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`, `src/lib/api.ts`, `src/lib/RunScreen.svelte`

**Interfaces:**
- Consumes: `budget::today_hits(pool)` (budget.rs:47), `AppState.cfg.read().await.soft_limit`.
- Produces: command `budget_today() -> BudgetToday { hits: i64, soft_limit: i64 }`.

- [ ] **Step 1: Check for prior art** — `grep -n "today_hits\|budget" src-tauri/src/commands.rs src/lib/api.ts`. If a budget command already exists, skip to Step 3 and only add the UI.

- [ ] **Step 2: Implement the command**

```rust
#[derive(serde::Serialize)]
pub struct BudgetToday { pub hits: i64, pub soft_limit: i64 }

#[tauri::command]
pub async fn budget_today(state: State<'_, AppState>) -> Result<BudgetToday, AppError> {
    Ok(BudgetToday {
        hits: budget::today_hits(&state.pool).await?,
        soft_limit: state.cfg.read().await.soft_limit,
    })
}
```

Register in `lib.rs`. (No new DB test — `today_hits` is already covered; the command is a trivial join of two tested reads.)

- [ ] **Step 3: UI** — `api.ts`: `budgetToday(): Promise<{hits: number, soft_limit: number}>`. `RunScreen.svelte`: load on mount and after each run completes; render one line above the run list: `Bloomberg hits today: {hits.toLocaleString()} / soft limit {soft_limit.toLocaleString()}`, styled amber when `hits > soft_limit` (reuse the existing warning class in that file).

- [ ] **Step 4: Verify** — `npm run check` → 0 errors; `cargo test` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src/lib/api.ts src/lib/RunScreen.svelte
git commit -m "feat: today's hit usage visible in Run screen"
```

---

### Task 4: Gap auto-backfill after downtime

**Files:**
- Modify: `src-tauri/src/orchestrator.rs` (new `run_gap_backfill` / `run_gap_backfill_with`), `src-tauri/src/scheduler.rs` (tick, :120-228)
- Test: `src-tauri/tests/ops.rs`

**Interfaces:**
- Consumes: `scheduler::detect_gaps(pool, view_id, lookback_days, today) -> AppResult<Vec<Gap>>` (scheduler.rs:286; `Gap` at :266 — read the struct fields first), `scheduler::previous_weekday` (:83), `budget::{estimate_backfill_hits, today_hits, check_level, BudgetLevel}`, `run_backfill_with`'s internals (load_view + execute with `kind='backfill'`).
- Produces:
  - `pub const GAP_LOOKBACK_DAYS: i64 = 10;` (scheduler.rs — the manual UI keeps its 30).
  - `pub enum GapBackfillOutcome { Nothing, Ran { runs: u64, days: u64 }, NeedsConfirmation { estimated: i64, today_total: i64 }, AlreadyAttemptedToday }` (orchestrator.rs).
  - `orchestrator::run_gap_backfill(pool, cfg, view_id, today: NaiveDate) -> AppResult<GapBackfillOutcome>` and a `run_gap_backfill_with<F: DataFetcher>` twin (the `_with` variant skips the live post-run hooks, exactly like the existing `_with` trio).
  - Contract: gaps are detected up to `previous_weekday(previous_weekday_target)` — i.e. strictly before the day today's EOD will fetch, else yesterday always looks like a gap. Total estimate over all gaps is gated **once**: any level above `BudgetLevel::Ok` runs nothing and returns `NeedsConfirmation` (a scheduler cannot click a confirm box; same doctrine as verify). One attempt per day: any `kind='backfill' AND trigger_kind='scheduled'` run started today (any status) short-circuits to `AlreadyAttemptedToday`. Gap runs use `trigger='scheduled'` and never suppress the day's EOD (`already_ran_today` counts only eod/verify — do not change it).

- [ ] **Step 1: Write the failing tests** (append to `src-tauri/tests/ops.rs`; reuse the gap fixtures of `tests/pipeline.rs:291-420` — read them first, they build views with holey observation histories):

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn gap_backfill_fills_missed_weekdays_and_records_scheduled_backfill_runs() {
    // Fixture: instrument with current obs for 2026-08-13 (Thu) and 2026-08-18
    // (Tue), nothing for Fri 08-14 and Mon 08-17; today = Wed 2026-08-19.
    // Fetcher: a mock DataFetcher serving cells for any requested day
    // (mirror tests/pipeline.rs's canned fetchers).
    let out = orchestrator::run_gap_backfill_with(&pool, &cfg, &fetcher, vid, d("2026-08-19"))
        .await.unwrap();
    match out {
        orchestrator::GapBackfillOutcome::Ran { days, .. } => assert_eq!(days, 2),
        other => panic!("expected Ran, got {other:?}"),
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM run WHERE view_id = $1 AND kind = 'backfill' AND trigger_kind = 'scheduled'")
        .bind(vid).fetch_one(&pool).await.unwrap();
    assert!(n >= 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn gap_backfill_stops_at_the_soft_limit_instead_of_confirming_itself() {
    // Same fixture, but cfg.soft_limit = 0 so any estimate lands in SoftWarn.
    let out = orchestrator::run_gap_backfill_with(&pool, &cfg_zero, &fetcher, vid, d("2026-08-19"))
        .await.unwrap();
    assert!(matches!(out, orchestrator::GapBackfillOutcome::NeedsConfirmation { .. }));
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM run WHERE view_id = $1 AND kind = 'backfill' AND trigger_kind = 'scheduled'")
        .bind(vid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0, "nothing may run past Ok without a human");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn gap_backfill_attempts_at_most_once_per_day() {
    // Run once (Ran), run again same day -> AlreadyAttemptedToday, run count unchanged.
}
```

- [ ] **Step 2: Run to verify failure** — compile error: `run_gap_backfill_with`/`GapBackfillOutcome` missing.

- [ ] **Step 3: Implement** in orchestrator.rs (next to the other entry points):

```rust
pub enum GapBackfillOutcome {
    Nothing,
    Ran { runs: u64, days: u64 },
    NeedsConfirmation { estimated: i64, today_total: i64 },
    AlreadyAttemptedToday,
}

pub async fn run_gap_backfill_with<F: DataFetcher>(pool: &PgPool, cfg: &PipelineConfig,
    fetcher: &F, view_id: i64, today: NaiveDate) -> AppResult<GapBackfillOutcome>
{
    // One attempt per day, any status: a failing gap must not retry in a loop.
    let attempted: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM run
         WHERE view_id = $1 AND kind = 'backfill' AND trigger_kind = 'scheduled'
           AND started_at::date = CURRENT_DATE")
        .bind(view_id).fetch_one(pool).await?;
    if attempted > 0 { return Ok(GapBackfillOutcome::AlreadyAttemptedToday); }

    let horizon = crate::scheduler::previous_weekday(crate::scheduler::previous_weekday(today));
    let gaps = crate::scheduler::detect_gaps(pool, view_id, crate::scheduler::GAP_LOOKBACK_DAYS,
        horizon).await?;
    if gaps.is_empty() { return Ok(GapBackfillOutcome::Nothing); }

    // Gate the whole batch once, at Ok only -- a scheduler cannot click confirm.
    // (estimate per gap: that instrument's fields x weekdays; reuse load_view +
    //  estimate_backfill_hits per gap, summed)
    let today_total = crate::budget::today_hits(pool).await?;
    if !matches!(crate::budget::check_level(estimated, today_total, cfg.soft_limit),
                 crate::budget::BudgetLevel::Ok) {
        return Ok(GapBackfillOutcome::NeedsConfirmation { estimated, today_total });
    }

    // One run per gap range, single-instrument scope, kind 'backfill',
    // trigger 'scheduled' -- reuse the run_backfill_with body per gap.
    ...
    Ok(GapBackfillOutcome::Ran { runs, days })
}
```

(`...`/`estimated` assembly: reuse `run_backfill_with`'s exact load/estimate/execute sequence per gap with `only = Some(&[gap.instrument_id])`, `trigger = "scheduled"` — read orchestrator.rs:426-461 and factor a shared helper if the duplication is more than a few lines. `previous_weekday` and `detect_gaps` visibility: make `pub` if not already.) The live `run_gap_backfill` twin wraps it with `BlpapiFetcher` and the same post-run hooks the other live wrappers use.

In `scheduler::tick`, at the point where a schedule is due and `!already_ran_today` (before the verify/EOD decision at :179): call `run_gap_backfill`, prefix its outcome into the `last_result` text (`"gap backfill: 2 days; "` / `"gaps need confirmation (N est. hits); "` / silent for Nothing/AlreadyAttempted), and continue to the normal run — errors are per-schedule-isolated like everything else in tick.

- [ ] **Step 4: Run tests** — `cargo test --no-fail-fast --test ops --test pipeline -- --ignored` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/orchestrator.rs src-tauri/src/scheduler.rs src-tauri/tests/ops.rs
git commit -m "feat: scheduler backfills missed days after downtime -- budget-gated, once per day, never self-confirms"
```

---

### Task 5: Overrides plumbing + NIL-fill non-trading evidence

**Files:**
- Modify: `src-tauri/src/fetch.rs` (`RequestSpec` :97-108, `Override` struct), `src-tauri/scripts/blp_fetch.py` (:441-457 request settings; response parsing), `src-tauri/scripts/test_blp_fetch.py`
- Test: inline serde test in fetch.rs; canned-response test in test_blp_fetch.py

**Interfaces:**
- Consumes: sidecar's existing override support (`spec["overrides"]` = list of `{"fieldId", "value"}`, blp_fetch.py:163-166/:479-491) and its problem-list channel (fetch.rs:373-389 `map_response` → ingest Rule A at ingest.rs:182-187).
- Produces: `pub struct Override { pub field_id: String, pub value: String }` serialized `{"fieldId","value"}` (serde rename); `RequestSpec.overrides: Vec<Override>` with `#[serde(skip_serializing_if = "Vec::is_empty", default)]`, empty in `plan_requests` for now (CDR codes are a deferred live probe — spec Open Question 3). Sidecar historical requests switch `nonTradingDayFillOption` `ACTIVE_DAYS_ONLY` → `NON_TRADING_WEEKDAYS` plus `nonTradingDayFillMethod = NIL_VALUE`; a fieldData row bearing a date but none of the requested field values is emitted as problem `{security, date, code: "no_data", detail: "non-trading day (NIL fill)"}`. Rust ingest/quality/detect_gaps need **zero** change.

- [ ] **Step 1: Write the failing Rust serde test** (inline in fetch.rs tests):

```rust
#[test]
fn overrides_serialize_in_sidecar_shape_and_vanish_when_empty() {
    let mut spec = /* any RequestSpec fixture from the neighbouring tests */;
    assert!(!serde_json::to_string(&spec).unwrap().contains("overrides"));
    spec.overrides.push(Override { field_id: "CDR".into(), value: "US".into() });
    let js = serde_json::to_value(&spec).unwrap();
    assert_eq!(js["overrides"][0]["fieldId"], "CDR");
    assert_eq!(js["overrides"][0]["value"], "US");
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test overrides_serialize` → compile error (no field).

- [ ] **Step 3: Implement the Rust side** — add the struct + field with serde attributes as specified; update every `RequestSpec { … }` literal in fetch.rs (and tests) with `overrides: Vec::new()`.

- [ ] **Step 4: Write the failing Python test** (in `src-tauri/scripts/test_blp_fetch.py`, mirroring its existing canned-response fixtures — read the file first): a historical response containing one security with two fieldData rows — one with a value, one with only a `date` — must yield one cell and one problem `("no_data", "non-trading day (NIL fill)")` for the dated empty row. Run: `python -m pytest src-tauri/scripts/test_blp_fetch.py -k nil` → FAIL (row silently dropped today).

- [ ] **Step 5: Implement the sidecar side** — in blp_fetch.py's historical request builder (:441-457): `nonTradingDayFillOption = "NON_TRADING_WEEKDAYS"`, add `nonTradingDayFillMethod = "NIL_VALUE"` (adjustment flags stay all-false). In the historical response parser: a row with a `date` but none of the requested fields present → append the problem above instead of skipping. Run the full sidecar suite: `python -m pytest src-tauri/scripts` → PASS.

- [ ] **Step 6: Run everything** — `cargo test` → PASS; `cargo test --no-fail-fast -- --ignored` → PASS (Rule A tests in `tests/pipeline.rs` already pin no_data → non_trading_day).

- [ ] **Step 7: Note the live verification** in the plan-tracking doc / commit body: on the next Terminal session, confirm a known exchange holiday comes back as a NIL row and lands in `non_trading_day`, and check how a trading-halted (but open-exchange) day presents — spec Open Question 1.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/src/fetch.rs src-tauri/scripts/blp_fetch.py src-tauri/scripts/test_blp_fetch.py
git commit -m "feat: NIL-fill non-trading evidence + request override plumbing -- holidays stop masquerading as silence"
```

---

### Task 6: Migration 0013 — `current_eod` view for downstream consumers

**Files:**
- Create: `src-tauri/migrations/0013_current_eod_view.sql`
- Test: `src-tauri/tests/ops.rs`

**Interfaces:**
- Produces: SQL view `current_eod(instrument_id, label, mnemonic, obs_date, value_num, value_text, currency, run_id, believed_since)` — current belief only, raw layer, EOD granularity. Downstream reads it with plain SQL, no bitemporal knowledge.

- [ ] **Step 1: Write the failing test** (append to `src-tauri/tests/ops.rs`; reuse the `tests/dataview.rs` scaffold idiom — read :12 first):

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn current_eod_shows_current_belief_only() {
    let pool = common::pool().await;
    // Scaffold instrument + field + run; insert obs 2026-08-14 = 100.0,
    // then supersede it (close system_to, insert 101.0) -- mirror the
    // supersession fixture in tests/dataview.rs.
    let rows: Vec<(f64, Option<String>)> = sqlx::query_as(
        "SELECT value_num, currency FROM current_eod
         WHERE instrument_id = $1 AND mnemonic = $2 AND obs_date = '2026-08-14'")
        .bind(iid).bind(&mnemonic).fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 1, "exactly one current belief per day");
    assert_eq!(rows[0].0, 101.0);
}
```

- [ ] **Step 2: Run to verify failure** — relation `current_eod` does not exist.

- [ ] **Step 3: Write the migration** — `src-tauri/migrations/0013_current_eod_view.sql`:

```sql
-- P10: the one query downstream consumers need, without understanding
-- bitemporality. Current belief, raw layer, EOD granularity. Adjusted and
-- stitched series stay app-level (they are mode-parameterised).
CREATE VIEW current_eod AS
SELECT o.instrument_id,
       be.label,
       f.mnemonic,
       o.obs_date,
       o.value_num,
       o.value_text,
       o.currency,
       o.run_id,
       o.system_from AS believed_since
FROM observation o
JOIN field_def f  ON f.id = o.field_id
LEFT JOIN book_entry be ON be.instrument_id = o.instrument_id
WHERE o.system_to = 'infinity'
  AND o.layer = 'raw'
  AND o.granularity = 'eod';
```

`touch src-tauri/tests/common/mod.rs`; verify `i/lf`.

- [ ] **Step 4: Run tests** — `cargo test --test ops -- --ignored` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/migrations/0013_current_eod_view.sql src-tauri/tests/ops.rs src-tauri/tests/common/mod.rs
git commit -m "feat(db): current_eod view -- downstream reads one flat table, migration 0013"
```

---

### Task 7: Connection settings — DB URL + Bloomberg host/port in the UI

**Files:**
- Modify: `src-tauri/src/commands.rs` (`AppConfig` :13-31, `pipeline_cfg` :48-57), `src-tauri/src/lib.rs` (:42-44 startup), `src-tauri/src/orchestrator.rs` (`PipelineConfig` :21-28), `src-tauri/src/fetch.rs` (`SidecarPayload` :111-115), `src-tauri/src/master_fetch.rs` (its payload struct — find it near the request builders)
- Modify: `src/lib/api.ts`, `src/lib/SettingsScreen.svelte`

**Interfaces:**
- Consumes: sidecar's `payload.get("host", DEFAULT_HOST)` / `payload.get("port", DEFAULT_PORT)` (blp_fetch.py:521-522) — zero Python changes.
- Produces: `AppConfig` gains `#[serde(default)] pub database_url: Option<String>`, `#[serde(default)] pub blp_host: Option<String>`, `#[serde(default)] pub blp_port: Option<u16>` (old config.json parses unchanged). `PipelineConfig` gains `blp_host: Option<String>`, `blp_port: Option<u16>` (filled by `pipeline_cfg`). `SidecarPayload` and the master-fetch payload gain `#[serde(skip_serializing_if = "Option::is_none")] host` / `port`. Startup DB URL precedence: **config.json → `BLOOM_DATABASE_URL` env → hardcoded default** (UI-set beats env: the user who edits the UI must see an effect); takes effect on restart.

- [ ] **Step 1: Write the failing unit tests** (inline in commands.rs / fetch.rs test modules):

```rust
#[test]
fn old_config_json_still_parses() {
    let cfg: AppConfig = serde_json::from_str(
        r#"{"data_dir":"C:\\bloomdata","soft_limit":100000,"request_timeout_s":120,"python_path":"python"}"#)
        .unwrap();
    assert_eq!(cfg.database_url, None);
    assert_eq!(cfg.blp_port, None);
}

#[test]
fn sidecar_payload_carries_host_only_when_set() {
    // None -> keys absent (sidecar falls back to localhost:8194);
    // Some -> {"host":"10.0.0.5","port":9194} present.
}
```

- [ ] **Step 2: Run to verify failure** — compile errors on the new fields.

- [ ] **Step 3: Implement.** Fields as specified; `lib.rs:42-44` becomes:

```rust
let cfg = load_config();
let url = cfg.database_url.clone()
    .or_else(|| std::env::var("BLOOM_DATABASE_URL").ok())
    .unwrap_or_else(|| "postgres://postgres:postgres@localhost/bloomdata".into());
```

`pipeline_cfg` copies host/port into `PipelineConfig`; `BlpapiFetcher` (orchestrator.rs:72-96) and `BlpapiMasterFetcher` put them on their payloads. Update the `impl Default for AppConfig` with `None`s.

- [ ] **Step 4: UI** — SettingsScreen Configuration section gains: `database_url` text input with helper text "takes effect after restart; empty = BLOOM_DATABASE_URL env or localhost default", `blp_host` text ("empty = localhost"), `blp_port` number ("empty = 8194"). They ride the existing `saveSettings` round-trip — extend the `AppConfig` type in api.ts.

- [ ] **Step 5: Verify** — `cargo test` → PASS; `npm run check` → 0 errors; `cargo test --no-fail-fast -- --ignored` full sweep → PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src-tauri/src/orchestrator.rs src-tauri/src/fetch.rs src-tauri/src/master_fetch.rs src/lib/api.ts src/lib/SettingsScreen.svelte
git commit -m "feat: DB and Bloomberg connection settings in the UI -- config.json beats env beats default"
```

---

## Post-wave live smoke (user + Terminal, after merge)

1. Holiday evidence: pick a market with a recent holiday, backfill across it, confirm a `non_trading_day` row (source `no_data`) instead of a `quality_no_response`.
2. Downtime drill: skip a scheduled day (or fake it by deleting no data — just pick a view with a real hole), watch the next tick write "gap backfill: N days" into the schedule's last_result and the runs appear as `backfill/scheduled`.
3. `current_eod`: `SELECT * FROM current_eod LIMIT 20` in psql matches the Data tab.
4. CI badge green on master.

## Self-review checklist

- Spec 10.1→Task 1, 10.2→Tasks 2-3, 10.3→Task 4, 10.4→Task 5, 10.5→Task 6, 10.6→Task 7. Covered; hard cap explicitly absent (Global Constraints).
- Type consistency: `Override { field_id, value }` (Task 5) matches the serde test; `GapBackfillOutcome` variants match between test and implementation; `BudgetToday` field names match api.ts usage.
- Fixture `// ...` blocks always name the concrete file whose scaffold to copy; assertions are always spelled out.
