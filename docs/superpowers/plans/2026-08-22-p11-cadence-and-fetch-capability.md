# P11: Publication Cadence & Fetch Capability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Teach the pipeline when data is supposed to exist and which wire path can fetch it, so non-daily and history-unentitled series (monthly-NAV funds, individual bonds, irregular series) are collected correctly instead of generating waste, fake evidence, and silent holes — plus the weekly identity sweep that retires dead instruments.

**Architecture:** One new concept — effective cadence `COALESCE(field_def.cadence, asset_class.default_cadence)` plus `field_def.fetch_via` — threaded through six seams: the sidecar (periodicity passthrough), the planner (fetch-when-due partitions), gap detection (period-shaped gaps + the coverage-predicate fix), evidence recording (daily-history-only gating + the NIL-streak alarm), verify (per-cadence windows), and a weekly identity sweep feeding the existing `retire_path`. Everything `daily × history` is bit-for-bit today's behaviour.

**Tech Stack:** Rust (sqlx/Postgres, Tauri 2), Svelte 5, Python (BLPAPI sidecar).

**Spec:** `docs/superpowers/specs/2026-08-22-p11-cadence-and-fetch-capability-design.md`. Read it first — its probe findings F1-F8 are load-bearing for several tasks and are cited by number below.

## Global Constraints

- **NO hard budget cap** — standing user decision 2026-08-20. Soft limit warns; `HardConfirm` gates at 2× soft; the scheduler never auto-confirms past `Ok`.
- `run` and `hit_ledger` rows are never rewritten or deleted. Old migration files are never edited.
- **Fresh-DB stance (user ruling 2026-08-22):** current DB contents are disposable; migration 0014 is plain DDL with freely chosen defaults — but it still ships as a new migration file (sqlx replays the chain on fresh DBs).
- Migration files MUST be LF-only (`git ls-files --eol src-tauri/migrations` shows `i/lf`); after adding one, `touch src-tauri/tests/common/mod.rs`.
- DB integration tests: `#[ignore = "requires postgres"]`, `common::pool()`, `common::uniq()`. Commands from `src-tauri/`: `cargo test`, `cargo test --no-fail-fast -- --ignored`. Known permanent bloom_test failure: `smoke_real_bloomberg_end_to_end`. Frontend: `npm run check`, 0 errors (1 known ImportDiff.svelte warning).
- The sidecar is P0-measured: never change request building or parsing without a canned-response test in `src-tauri/scripts/test_blp_fetch.py` (run: `python -m pytest test_blp_fetch.py` or `python test_blp_fetch.py` from `src-tauri/scripts/`).
- Every commit message ends with: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- House style: `//!` module docs explain WHY; advisory sub-steps log-and-continue.

## File Structure

- Create: `src-tauri/migrations/0014_cadence_and_fetch_capability.sql`
- Create: `src-tauri/tests/cadence.rs` (planner partitions, period gaps, evidence gating, identity sweep)
- Already committed with this plan: `src-tauri/scripts/fixtures/live-2026-08-22-nilfill-multiasset-history.json`, `live-2026-08-22-bond-allnil-history.json` (raw sidecar captures from the probe)
- Modify: `src-tauri/scripts/blp_fetch.py` (periodicity passthrough), `test_blp_fetch.py` (fixture replay + request-building tests)
- Modify: `src-tauri/src/fetch.rs` (RequestSpec.periodicity, plan_requests partitions, dispatched_hits), `budget.rs` (periods_between), `views.rs` (cadence/fetch_via in field lookups), `orchestrator.rs` (due logic, period backfill), `scheduler.rs` (detect_gaps dispatch, verify windows, identity-sweep slot), `ingest.rs` (evidence gating, nil_streak), `quality.rs` (publication_overdue), `master_fetch.rs` or new `identity.rs` (sweep fetcher), `commands.rs` (class/field editors)
- Modify: `.github/workflows/ci.yml` (branch trigger), `src/lib/api.ts`, `src/lib/SettingsScreen.svelte`, field-editor component, gap-report rendering

---

### Task 1: Migration 0014 + model plumbing + CI trigger

**Files:**
- Create: `src-tauri/migrations/0014_cadence_and_fetch_capability.sql`
- Modify: `.github/workflows/ci.yml`, Rust structs that mirror `asset_class` / `field_def` (follow the P9 pattern from migration 0011's wave)

**Interfaces:**
- Produces: `asset_class.default_cadence TEXT NOT NULL DEFAULT 'daily'`, `asset_class.cadence_grace_days INTEGER NOT NULL DEFAULT 10 CHECK (cadence_grace_days >= 0)`, `asset_class.identity_sweep TEXT NOT NULL DEFAULT 'none' CHECK IN ('none','market_status','maturity')`, `field_def.cadence TEXT NULL` (same cadence CHECK), `field_def.fetch_via TEXT NOT NULL DEFAULT 'history' CHECK IN ('history','reference')`. Cadence CHECK set: `('daily','weekly','monthly','quarterly','irregular')`.
- Effective cadence everywhere = `COALESCE(f.cadence, ac.default_cadence)` (the `qc_stale_days` idiom).

- [ ] **Step 1:** Write the migration (plain DDL; equity-shaped defaults keep every existing class and field behaving exactly as today). LF-only; `touch src-tauri/tests/common/mod.rs`.
- [ ] **Step 2:** Extend the Rust row structs + CRUD used by the Settings editors (whatever P9 touched for `corp_actions_capable` — find and mirror). RED: a `cadence.rs` test creating a class with `default_cadence='monthly'` and a field overriding `cadence='daily'`, asserting the COALESCE reads back correctly through the same query path production uses. GREEN. `identity_sweep='market_status'` default note: default is `'none'` — equity classes must OPT IN via settings; do not flip existing classes in the migration.
- [ ] **Step 3:** CI: change `push.branches` to `[master, 'p1*-**']` so this and future waves get CI (P10's `p10-**` would not match `p11-…`).
- [ ] **Step 4:** Full local suite green. Commit: `feat(db): cadence, fetch_via and identity_sweep capability columns -- migration 0014`

### Task 2: Sidecar periodicity passthrough + live-fixture canned tests

**Files:**
- Modify: `src-tauri/scripts/blp_fetch.py`, `src-tauri/scripts/test_blp_fetch.py`
- Use: the two committed `fixtures/live-2026-08-22-*.json` captures

**Interfaces:**
- Consumes: optional `"periodicity"` on a history request spec (absent = DAILY).
- Produces: `r.set("periodicitySelection", spec.get("periodicity") or "DAILY")`; the NIL-fill option pair is set ONLY when effective periodicity is DAILY (spec F3). `validate_request_spec` accepts only `DAILY|WEEKLY|MONTHLY|QUARTERLY` when the key is present (reject unknown strings loudly — Bloomberg launders bad enum values into empty results).
- Parsing: UNCHANGED. Monthly rows arrive in the same fieldData shape (F3).

- [ ] **Step 1 (RED):** Request-building tests: (a) no periodicity key → DAILY + NIL pair set (today's exact request, byte-comparable); (b) `"periodicity":"MONTHLY"` → MONTHLY, NIL pair ABSENT; (c) `"periodicity":"weekly"`/garbage → validation error naming the spec index.
- [ ] **Step 2 (RED):** Fixture replay tests using `--replay` semantics on the two committed captures: multi-asset NIL capture → exactly 45 observations + 4 problems, with the four `no_data` problems dated 2026-07-03 for SPX/AAPL/CL1 and the invalid_security for the bad bond ticker; bond all-NIL capture → exactly 0 observations + 25 problems (24 `no_data` + 1 `invalid_security`). These pin the live wire truth captured 2026-08-22.
- [ ] **Step 3 (GREEN):** Implement. All sidecar tests green (34 existing + new).
- [ ] **Step 4:** Commit: `feat(sidecar): periodicity passthrough, NIL fill pinned to DAILY -- live wire fixtures from the 2026-08-22 probe`

### Task 3: Planner partitions + budget model

**Files:**
- Modify: `src-tauri/src/fetch.rs`, `src-tauri/src/budget.rs`, `src-tauri/src/views.rs`
- Test: `src-tauri/tests/cadence.rs` (append), fetch.rs unit tests

**Interfaces:**
- `RequestSpec.periodicity: Option<String>` with `#[serde(skip_serializing_if = "Option::is_none")]` — wire bytes for existing daily requests MUST NOT change (P10 host/port discipline; assert via the existing serialization tests' pattern).
- `views::view_fields` rows carry effective cadence + fetch_via.
- `plan_requests` partitions (instrument, field) pairs: daily×history → today's history spec (unchanged); **reference-via numeric fields join the existing text-field reference spec** (`fetch.rs:234-261` seam — same request, more fields); periodic×history pairs are EXCLUDED here and planned only by the due-logic in Task 4 (plan_requests plans a single run's work; periodic fetches are not part of every run).
- `budget::periods_between(start, end, cadence) -> i64`; `dispatched_hits` uses it for specs carrying a periodicity, `weekdays_between` otherwise (`fetch.rs:282` today).

- [ ] **Step 1 (RED):** Unit tests: reference-via numeric field lands in the reference spec and NOT in history; a periodic field appears in no spec from `plan_requests`; serialization: spec without periodicity produces byte-identical JSON to today (extend `eod_splits_numeric_to_history_and_text_to_reference` and the P10 "vanish when empty" test pattern); `dispatched_hits` on a MONTHLY spec over 3 months = securities × fields × 3.
- [ ] **Step 2 (GREEN):** Implement. `periods_between`: count period-ends in [start, end] (month-/quarter-/week-ends; weekly = Fridays).
- [ ] **Step 3:** Full suite green. Commit: `feat(fetch): cadence/fetch_via planner partitions and period-aware budget math`

### Task 4: Due logic, period gaps, coverage fix, verify windows

**Files:**
- Modify: `src-tauri/src/orchestrator.rs`, `src-tauri/src/scheduler.rs`
- Test: `src-tauri/tests/cadence.rs` (append)

**Interfaces:**
- **Due logic** (new orchestrator fn, called from `run_eod`'s flow after the daily leg plans): for each periodic×history (instrument, field): most recently ENDED period lacking a current observation, not already attempted today (a `hit_ledger`/run-scoped attempt record — decide with the reviewer; the once-a-day predicate must survive restarts, so derive it from stored rows, not memory) → one ranged history spec for the whole period with matching periodicity. Never fetch an unfinished period (F3).
- **Coverage predicate fix** (`detect_gaps`, scheduler.rs:326/360): `expected` counts ONLY daily×history non-text fields. THE deliberate consequence: a date is never permanently uncovered because of a field daily backfill cannot supply — this closes the P10-review "permanently-partial days re-bought daily" defect for mixed views. Pin with a test: instrument with one daily field present + one monthly field absent on a date → NOT a gap.
- **Period gaps**: new detection arm — for each periodic×history field, completed periods within lookback (2 cycles) with no print and `today > period_end + grace` → `Gap` with period label. Backfill for a period gap = the same single ranged request as due logic (share the code path).
- **Verify windows**: daily fields keep `verify_window_start` (5 weekdays); periodic fields re-read last 2 COMPLETED periods in the verify run; reference-via and irregular fields excluded from verify.
- `GapBackfillOutcome` doctrine unchanged: gate `!= Ok` → NeedsConfirmation runs nothing; once/day; corp-action leg priced once per batch (89f1adb doctrine — do not regress it).

- [ ] **Step 1 (RED):** Postgres tests: (a) monthly field, period ended, no obs, grace passed → due produces exactly one spec with periodicity MONTHLY covering the period; (b) same but obs exists → nothing; (c) unfinished current period → nothing; (d) the coverage-predicate pin above; (e) period-gap detection honors grace and the 2-cycle lookback; (f) verify plan for a mixed view contains the 5-weekday daily leg + one 2-period monthly leg and no reference-via fields.
- [ ] **Step 2 (GREEN):** Implement. Keep every `daily×history` code path byte-identical — the partition dispatch wraps existing functions, it does not rewrite them.
- [ ] **Step 3:** Full suite incl. `--ignored` green. Commit: `feat(scheduler): fetch-when-due for periodic series, period-shaped gaps, cadence verify windows -- daily path untouched`

### Task 5: Evidence gating + NIL-streak + publication_overdue

**Files:**
- Modify: `src-tauri/src/ingest.rs`, `src-tauri/src/quality.rs`
- Test: `src-tauri/tests/cadence.rs` (append)

**Interfaces:**
- Rules A/B write `non_trading_day` ONLY when the fetch was daily-cadence, history-via (thread a flag through `record_non_trading_days`' request context; periodic/irregular/reference fetches record nothing).
- `nil_streak` quality finding (`severity='quality'`, code `nil_streak`): at ingest of a daily history fetch, if the instrument's trailing consecutive-weekday run of NIL/non-trading marks (current fetch + stored evidence) reaches 5, emit once per run with the span in detail. Evidence is STILL recorded (spec 11.6: the alarm is the human's signal; the evidence stops the machine re-buying junk — F6 is the disaster this prevents from being silent).
- `publication_overdue` quality finding: period late past grace (from Task 4's detection) surfaces as a run-level quality finding like other P7 findings — never flips run status by itself beyond the existing `quality_findings > 0 → partial` rule.

- [ ] **Step 1 (RED):** (a) periodic fetch with absent days → zero `non_trading_day` rows; (b) daily fetch, 5-weekday all-NIL streak assembled across two runs → exactly one `nil_streak` finding on the second run, evidence rows still present; (c) 4-day streak → no finding; (d) overdue monthly period → `publication_overdue` finding with period named.
- [ ] **Step 2 (GREEN):** Implement. `cargo test --no-fail-fast -- --ignored` green.
- [ ] **Step 3:** Commit: `feat(ingest): non-trading evidence gated to daily history; nil_streak and publication_overdue findings -- entitlement holes stop being silent`

### Task 6: Weekly identity sweep

**Files:**
- Modify: `src-tauri/src/scheduler.rs` (weekly slot, mirroring `verify_dow`/`last_verified_on` — add `identity_dow`/`last_identity_on` to `schedule` in migration 0014, Task 1 coordinates), sweep fetcher in `src-tauri/src/master_fetch.rs` (or new `identity.rs` if master_fetch is the wrong home — implementer's call, say why), `src-tauri/src/budget.rs` seam usage
- Test: `src-tauri/tests/cadence.rs` (append)

**Interfaces:**
- Per class with `identity_sweep != 'none'`: ONE batched ReferenceDataRequest over active instruments — `'market_status'` → MARKET_STATUS + INACTIVE_DATE; `'maturity'` → MATURITY + CALLED_DT + INACTIVE_DATE (field sets are probe-verified F5/F6; MARKET_STATUS is N/A outside equity-shaped classes, hence the per-class sets).
- Triggers: `'market_status'`: status ≠ ACTV or INACTIVE_DATE set; `'maturity'`: any date ≤ today. Route into the EXISTING P9 lifecycle (`retire_path` / M&A investigation per `ma_capable`) — this task builds the fetch + dispatch, not new lifecycle.
- **Per-field tolerance (spec F9):** `field_not_applicable` on any sweep field for a given security is normal (open-end funds lack INACTIVE_DATE); evaluate triggers on whichever fields returned, and only a security where ALL sweep fields fail is an anomaly (log-and-continue, advisory style).
- Hits at the wire seam via `budget::record_purpose_hits(purpose='identity', run_id NULL)` — corp-actions precedent, no estimate leg, no double count.
- Scheduler: never auto-confirms; sweep skipped (with note) when gate `!= Ok`; per-schedule isolation like every other tick branch.

- [ ] **Step 1 (RED):** Tests: (a) class with `'maturity'`, instrument with MATURITY yesterday (canned fetch outcome) → retire_path invoked, ledger row purpose `'identity'`; (b) `'none'` class → no request planned; (c) spot-shaped class default stays `'none'` (guard F5's settlement-date trap by construction).
- [ ] **Step 2 (GREEN):** Implement with a fetcher trait fake (the P9 test pattern); live wrapper wires the real sidecar.
- [ ] **Step 3:** Commit: `feat(lifecycle): weekly identity sweep retires matured and delisted instruments -- the P9 rider, probe-grounded`

### Task 7: UI + api

**Files:**
- Modify: `src/lib/api.ts`, `src/lib/SettingsScreen.svelte` (class editor columns), the field editor component (locate it), gap-report rendering, `src-tauri/src/commands.rs`

**Interfaces:**
- Class editor: `default_cadence` select, `cadence_grace_days` number, `identity_sweep` select.
- Field editor: `cadence` override select (blank = class default), `fetch_via` select; choosing `reference` shows: "Snapshot at run time, not an official close. Missed days cannot be backfilled."
- Book/onboarding hint near ticker entry: bonds need `/isin/<ISIN>` or CT/GT generics (spec F6/F8 wording).
- Gap report: period-shaped gaps render their period label.

- [ ] **Step 1:** Rust commands extended for the new columns (P9 asset-class editor pattern); `npm run check` 0 errors.
- [ ] **Step 2:** Svelte edits, brief helper texts (house tone: one plain sentence, no walls).
- [ ] **Step 3:** Commit: `feat(ui): cadence, fetch_via and identity sweep settings -- plus honest reference-snapshot and bond-onboarding hints`

---

## Post-wave live checks (need the Terminal, cannot be CI'd)

- [ ] Onboard `HFHSELA LX Equity` (daily-NAV fund, Lux holiday calendar — spec F9) and `DLVEEMEU Index` as daily instruments; confirm NIL evidence lands on Luxembourg holidays and the identity sweep tolerates the fund's missing INACTIVE_DATE.
- [ ] Onboard ONE genuinely monthly-NAV instrument when the user finds one (spec Open Question 1) with `default_cadence='monthly'`; watch two publication cycles; tune `cadence_grace_days` to the observed lag.
- [ ] Onboard CT10 Govt (or `/isin/US91282CRF04`) with `fetch_via='reference'` on the price fields and `identity_sweep='maturity'`; confirm daily snapshots land and no `non_trading_day` rows appear for it.
- [ ] Confirm `nil_streak` fires by pointing one throwaway history-via field at the bond (the F6 condition reproduced deliberately), then remove it.
- [ ] Watch the first scheduled identity sweep's ledger rows (purpose `'identity'`) and hit count.
