# Corporate Actions With Every Run Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix the live-confirmed `corp_action_current` duplicate-key crash (Hermès: two same-date dividend factors), contain per-instrument failures, surface Bloomberg's "field not applicable" answers, and make corporate actions refresh automatically with every EOD run and backfill so factors always sit beside prices (user decision 2026-08-21: "with every EOD run").

**Architecture:** Natural keys get a deterministic occurrence suffix inside one snapshot. `MasterFetcher::corp_actions` returns tables *and* sidecar problems. A tiny `corp_actions_na` registry (evidence-based, like `non_trading_day`) stops re-charging hits for securities Bloomberg says the fields don't apply to; a manual refresh retries and clears it. The live run wrappers (`run_eod` / `run_backfill`) call `corp_actions::refresh_view` after a completed run — same pattern as `auto_reresolve_after` — and inject the summary into `RunOutcome::Completed`.

**Tech Stack:** unchanged (Rust/sqlx/Tauri, Svelte 5, no sidecar changes).

**Spec:** live evidence 2026-08-21 (`scratchpad/ca_repro.json`): RMS FP Equity factor table has 5 duplicated `{date}|2|1` keys (ordinary + extraordinary dividend, same ex-date, different factors); YODA LN Equity answers `field_error: Field not applicable to security` for both fields. P3 design `docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md` governs storage; its §5 "no automatic fetches" is superseded by this plan and must be amended in Task 6.

## Global Constraints

- Append-only stores; hits charged at the wire seam; NO hard 500k cap (user decision, standing).
- Batch ≤ `fetch::MAX_SECURITIES_PER_REQUEST` (100); cost = securities × 2 per request.
- After adding a migration: `touch src-tauri/tests/common/mod.rs` (stale `sqlx::migrate!` gotcha).
- Integration tests `#[ignore = "requires postgres"]`, shared `bloom_test`, `common::uniq()`.
- A CA-refresh failure after a run must never fail the run itself.
- The empty-snapshot guard stays per (instrument, source_field).

---

### Task 1: Deduplicate natural keys inside one snapshot (crash fix)

**Files:** `src-tauri/src/corp_actions.rs` (parse_table + unit test), `src-tauri/tests/corp_actions.rs`.

**Interfaces:** `parse_table` keeps its signature; duplicate keys within the returned Vec get suffixes `|2`, `|3`… after a deterministic sort of the duplicates by canonical payload string. First occurrence keeps the bare key.

- [ ] **Step 1: failing unit test** in `corp_actions.rs::tests` — the real Hermès shape:

```rust
#[test]
fn same_day_twin_dividend_factors_get_distinct_keys() {
    // Live 2026-08-21: RMS FP pays an ordinary + extraordinary dividend with
    // one ex-date; both rows are operator 2 / flag 1 and differ only in factor.
    let t = SidecarBulkRows {
        security: "RMS FP Equity".into(), field: FACTOR_FIELD.into(),
        rows: serde_json::from_value(serde_json::json!([
            {"Adjustment Date": "2025-05-05", "Adjustment Factor": 0.994902,
             "Adjustment Factor Operator Type": 2.0, "Adjustment Factor Flag": 1.0},
            {"Adjustment Date": "2025-05-05", "Adjustment Factor": 0.995901,
             "Adjustment Factor Operator Type": 2.0, "Adjustment Factor Flag": 1.0}
        ])).unwrap(),
    };
    let acts = parse_table(&t);
    let mut keys: Vec<&str> = acts.iter().map(|a| a.natural_key.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["2025-05-05|2|1", "2025-05-05|2|1|2"],
               "duplicates disambiguated, deterministic");
    // Determinism: parsing again yields the same key for the same payload.
    let again = parse_table(&t);
    for a in &acts {
        let twin = again.iter().find(|b| b.payload == a.payload).unwrap();
        assert_eq!(twin.natural_key, a.natural_key);
    }
}
```

- [ ] **Step 2:** run `cargo test same_day_twin` → FAIL (both keys `2025-05-05|2|1`).
- [ ] **Step 3:** implement at the end of `parse_table`, before returning:

```rust
fn disambiguate_keys(actions: &mut [ParsedAction]) {
    use std::collections::HashMap;
    let mut groups: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, a) in actions.iter().enumerate() {
        groups.entry(a.natural_key.clone()).or_default().push(i);
    }
    for (key, mut idxs) in groups {
        if idxs.len() < 2 { continue; }
        // Sort duplicates by canonical payload so the suffix assignment is
        // stable across refreshes of the same snapshot.
        idxs.sort_by_key(|&i| actions[i].payload.to_string());
        for (n, &i) in idxs.iter().enumerate().skip(1) {
            actions[i].natural_key = format!("{key}|{}", n + 1);
        }
    }
}
```

called as `let mut out: Vec<_> = …collect(); disambiguate_keys(&mut out); out`.

- [ ] **Step 4:** unit test passes; existing corp_actions unit tests still green.
- [ ] **Step 5: failing integration test** in `tests/corp_actions.rs` — mock with the twin rows, `refresh` → `inserted == 2`; refresh again → `unchanged == 2`, still 2 open rows.
- [ ] **Step 6:** `cargo test --test corp_actions -- --ignored` green. Commit `fix: same-day duplicate corporate actions get occurrence-suffixed keys`.

### Task 2: One bad name must not abort the view refresh

**Files:** `src-tauri/src/corp_actions.rs`, `src-tauri/tests/corp_actions.rs`, `src-tauri/src/lib.rs` (nothing), `src/lib/api.ts` + `src/lib/ViewsScreen.svelte` (Task 5 picks up the label).

**Interfaces:** `ViewRefreshSummary` gains `pub failed: u64`. In `refresh_view`, the `apply_tables` call per instrument becomes:

```rust
match apply_tables(pool, *iid, &tables).await {
    Ok(s) => { /* sum as today */ }
    Err(e) => {
        sum.failed += 1;
        let _ = sqlx::query(
            "INSERT INTO ingest_issue (instrument_id, severity, code, detail)
             VALUES ($1,'error','corp_actions_failed',$2)")
            .bind(*iid).bind(e.to_string()).execute(pool).await;
    }
}
```

- [ ] **Step 1: failing integration test** — two members; mock returns for member 2 a table whose `field` is `"BOGUS_FIELD"` (violates the `source_field` CHECK on insert). Expect: member 1's rows committed, `failed == 1`, `instruments == 1`, issue `corp_actions_failed` present.
- [ ] **Step 2:** implement; suite green. Commit `fix: contain a per-instrument failure inside the view corp-actions refresh`.

### Task 3: Problems through the seam + not-applicable registry

**Files:** `src-tauri/migrations/0005_corp_actions_na.sql` (new), `src-tauri/src/master_fetch.rs` (trait, live, mock, unit test), `src-tauri/src/corp_actions.rs`, `src-tauri/src/commands.rs` (signature pass-through), `src-tauri/tests/corp_actions.rs`, `src-tauri/tests/bulk_import.rs` + `src-tauri/tests/resolution.rs` (stub fetchers).

**Migration 0005:**

```sql
-- Evidence that Bloomberg answered "Field not applicable to security" for
-- BOTH corp-action fields (live 2026-08-21: YODA LN Equity, accumulating
-- ETF). The automatic per-run refresh skips these instruments; a manual
-- refresh retries and, on success, deletes the row.
CREATE TABLE corp_actions_na (
  instrument_id BIGINT PRIMARY KEY REFERENCES instrument(instrument_id),
  noted_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  detail        TEXT NOT NULL
);
```

**Interfaces:** in `fetch.rs` nothing changes. In `master_fetch.rs`:

```rust
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CorpActionsTables {
    pub tables: Vec<crate::fetch::SidecarBulkRows>,
    pub problems: Vec<crate::fetch::SidecarProblem>,
}
fn corp_actions(&self, securities: &[String])
    -> impl Future<Output = AppResult<Answered<CorpActionsTables>>> + Send;
```

Live impl: `parsed.tables` from `resp["bulk_rows"]`, `parsed.problems` from `resp["problems"]`, `raw` = the whole `resp` (fuller audit than before). Mock gains `pub corp_actions_problems: serde_json::Value` (default `[]`). `SidecarProblem` needs `Clone` (add derive if missing). Stub fetchers in the two test files return `CorpActionsTables::default()`.

`refresh_view(pool, fetcher, view_id, as_of, skip_na: bool)`:
- when `skip_na`, exclude members present in `corp_actions_na` before building targets (no issue spam, no hits);
- after the fetch, per member: if it has **no tables** and every field in `CORP_ACTIONS_FIELDS` has a problem with `code == "field_error"` for its security → upsert `corp_actions_na` (`ON CONFLICT (instrument_id) DO UPDATE SET noted_at = now(), detail = EXCLUDED.detail`), insert `ingest_issue (severity 'info', code 'corp_actions_not_applicable', detail from the problem)`, count `sum.not_applicable += 1`, and do NOT call `apply_tables`;
- if it HAS tables → `DELETE FROM corp_actions_na WHERE instrument_id = $1` (recovery), then apply as today.
`ViewRefreshSummary` gains `pub not_applicable: u64`. Single-instrument `refresh` does the same NA bookkeeping (its fetch already returns problems).
Command `refresh_view_corp_actions` passes `skip_na = false` (a click is an explicit retry).

- [ ] **Step 1:** update the master_fetch mock unit test for the new type; run → FAIL (compile).
- [ ] **Step 2:** implement seam change end-to-end (`cargo test` compiles, unit green).
- [ ] **Step 3: failing integration tests** in `tests/corp_actions.rs`:
  1. `a_field_not_applicable_member_is_flagged_and_then_skipped` — member 2's mock problems = both fields `field_error`; `refresh_view(..., false)` → `not_applicable == 1`, `corp_actions_na` row exists, issue exists; then `refresh_view(..., true)` with a call-recording mock → the request string does NOT contain member 2's security.
  2. `a_recovered_security_clears_the_na_flag` — seed `corp_actions_na` by hand; mock now returns a factor table for it; `refresh_view(..., false)` → row inserted, `corp_actions_na` empty.
- [ ] **Step 4:** implement; `touch tests/common/mod.rs`; both suites green. Commit `feat: surface field-not-applicable corporate-action answers and stop re-charging them`.

### Task 4: Refresh with every EOD run and backfill

**Files:** `src-tauri/src/orchestrator.rs`, `src-tauri/src/corp_actions.rs` (nothing new), `src-tauri/src/budget.rs` (nothing — reuse `corp_actions_hit_cost`), `src-tauri/tests/pipeline.rs`.

**Interfaces:**
- `RunOutcome::Completed` gains `corp_actions: Option<crate::corp_actions::ViewRefreshSummary>` (starts `None`; serde skips nothing — api.ts marks it optional).
- New helper in orchestrator (testable estimate arithmetic stays in budget-style pure form):

```rust
/// 2 hits per member that will actually be requested (has a security, not
/// flagged not-applicable). Advisory for the pre-run gate; the seam still
/// charges what is actually sent.
async fn corp_actions_estimate(pool: &PgPool, view_id: i64) -> AppResult<i64> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM view_instrument vi
          WHERE vi.view_id = $1
            AND NOT EXISTS (SELECT 1 FROM corp_actions_na na
                             WHERE na.instrument_id = vi.instrument_id)")
        .bind(view_id).fetch_one(pool).await?;
    Ok(crate::master_fetch::corp_actions_hit_cost(n as usize))
}
```

- `run_eod_with` and `run_backfill_with`: `estimated` becomes price estimate `+ corp_actions_estimate(pool, view_id)` (backfill adds it ONCE, not per day).
- Live wrappers `run_eod` / `run_backfill`, after `auto_reresolve_after`:

```rust
async fn corp_actions_after(pool: &PgPool, cfg: &PipelineConfig, view_id: i64,
                            result: &mut AppResult<RunOutcome>) {
    if let Ok(RunOutcome::Completed { corp_actions, .. }) = result {
        let mf = crate::master_fetch::BlpapiMasterFetcher { cfg, pool };
        match crate::corp_actions::refresh_view(
            pool, &mf, view_id, chrono::Local::now().date_naive(), true).await {
            Ok(sum) => *corp_actions = Some(sum),
            Err(e) => eprintln!("corp-actions refresh after run failed: {e}"),
        }
    }
}
```

(the run stays Completed either way — a CA failure is reported, never fatal).

- [ ] **Step 1: failing integration test** in `tests/pipeline.rs`: `the_pre_run_gate_prices_in_corporate_actions` — view with 2 members, soft limit set so that price estimate alone passes but price+4 crosses into HardConfirm → `run_eod_with` (mock DataFetcher, unconfirmed) returns `NeedsConfirmation` with `estimated == price_est + 4`. Second assertion: seed one member into `corp_actions_na` → estimated drops by 2.
- [ ] **Step 2:** implement estimates + RunOutcome field + wrappers; fix any existing test asserting old `estimated` values.
- [ ] **Step 3:** full pipeline + corp_actions suites green. Commit `feat: corporate actions refresh automatically with every run, priced into the gate`.

### Task 5: UI — run summary line, view notice, api types

**Files:** `src/lib/api.ts`, `src/lib/RunScreen.svelte`, `src/lib/ViewsScreen.svelte`.

- [ ] `api.ts`: `ViewRefreshCorpActionsSummary` gains `failed: number; not_applicable: number;` `RunOutcome`'s Completed variant type gains `corp_actions?: ViewRefreshCorpActionsSummary | null`.
- [ ] `RunScreen.svelte`: after a completed run, when `corp_actions` is present render a thin line: `Corporate actions: a new, b amended, c withdrawn, d unchanged[, e unparsed][, f failed][, g not applicable][, h skipped]`.
- [ ] `ViewsScreen.svelte`: extend the existing `caNotice` line with `failed` and `not applicable` counts (same conditional style).
- [ ] `svelte-check` 0 errors. Commit `feat(ui): corporate-action results on the run summary and view notice`.

### Task 6: Full verification + docs + memory

- [ ] `cargo test` (unit) + every `--ignored` suite except `smoke_real` green; `svelte-check` clean.
- [ ] P3 design doc: §2 gains a "Shipped 2026-08-21 — automatic with every run" paragraph; §5 non-goal "No automatic or scheduled corp-action fetches" struck and annotated (superseded by user decision 2026-08-21); §3 notes the occurrence-suffix rule for same-day twins.
- [ ] Update memory `bloomberg-pipeline-status.md`.
- [ ] Commit `docs: corporate actions are pipeline data, refreshed with every run`.
