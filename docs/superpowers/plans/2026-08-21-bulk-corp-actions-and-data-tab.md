# Bulk Corporate-Actions Refresh + Data Tab Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** (1) Refresh corporate actions for a whole view in batched Bloomberg requests instead of one stock at a time; (2) a read-only **Data** tab that renders what the database actually stores — observations with basis and supersession history, corporate actions with their verbatim Bloomberg payload — plus CSV export, so accuracy can be checked against the Terminal without psql.

**Architecture:** `MasterFetcher::corp_actions` widens from one security to a slice (the sidecar's `bulk_reference` already accepts many securities; `bulk_rows` come back keyed by security). The single-instrument diff logic is extracted into a per-instrument apply step reused by `refresh` and the new `refresh_view`. The Data tab is read-only commands over existing tables — zero Bloomberg calls — with backend CSV export following the Book screen's typed-path pattern.

**Tech Stack:** unchanged (Rust/sqlx/Tauri, Svelte 5, existing sidecar — no sidecar changes).

**Spec:** user request 2026-08-21 ("let the user request those data for a whole set of stocks", then the Data tab as proposed: stored observations with superseded toggle, corp-action payload view, CSV export). P3 design `docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md` still governs storage semantics.

## Global Constraints

- Same as the 2026-08-20 plans: append-only stores, P0-confirmed mnemonics only, hits charged at the wire seam, no hard cap, migrations never edited after application, integration tests `#[ignore = "requires postgres"]` on shared `bloom_test` with `uniq()`.
- Batch at most 100 securities per Bloomberg request (`fetch::MAX_SECURITIES_PER_REQUEST` convention).
- View refresh cost = 2 hits × members with a current security, charged per request at the seam (securities × 2 fields).
- The Data tab makes **no** Bloomberg calls, ever.
- A view member with no current security is skipped and reported (`ingest_issue` `corp_actions_skipped`), mirroring `load_view`'s `no_security_today`.
- The empty-snapshot guard stays **per (instrument, source_field)**: in a batched response, a security that errored simply has no rows, and its local history must not be closed.

---

### Task 1: Widen the seam — `corp_actions(securities: &[String])`

**Files:** `src-tauri/src/master_fetch.rs` (trait, live impl, mock, tests), `src-tauri/src/corp_actions.rs` (caller), `src-tauri/tests/bulk_import.rs`, `src-tauri/tests/resolution.rs` (test fetchers).

**Interfaces:** trait method becomes
```rust
fn corp_actions(&self, securities: &[String])
    -> impl Future<Output = AppResult<Answered<Vec<SidecarBulkRows>>>> + Send;
```
`CORP_ACTIONS_HIT_COST` is replaced by `pub fn corp_actions_hit_cost(securities: usize) -> i64` (= securities × 2), mirroring `identity_hit_cost`. Live impl charges `corp_actions_hit_cost(securities.len())`.

- [x] Update the two master_fetch unit tests: cost test asserts `corp_actions_hit_cost(1) == 2`, `corp_actions_hit_cost(50) == 100`, `corp_actions_hit_cost(0) == 0`; mock test passes `&["AAPL US Equity".into()]` and records `corp_actions:AAPL US Equity` (join with `,` like `identity`). Run: FAIL (signature).
- [x] Implement: trait + live (`"securities": securities`) + mock + the two test fetchers in tests/ take slices. `corp_actions::refresh` calls `fetcher.corp_actions(&[security.clone()])`.
- [x] `cargo test` + `cargo test --test corp_actions -- --ignored` green. Commit `feat: corp_actions seam takes a batch of securities`.

### Task 2: `refresh_view` — batched, per-instrument diff

**Files:** `src-tauri/src/corp_actions.rs`, `src-tauri/tests/corp_actions.rs`.

**Interfaces:**
```rust
#[derive(Debug, Default, Serialize)]
pub struct ViewRefreshSummary {
    pub instruments: u64, pub skipped: u64,
    pub inserted: u64, pub amended: u64, pub withdrawn: u64,
    pub unchanged: u64, pub unparsed: u64,
}
pub async fn refresh_view<F: MasterFetcher>(pool, fetcher, view_id, as_of)
    -> AppResult<ViewRefreshSummary>;
```
Internals: extract the existing per-instrument diff body into
`async fn apply_tables(pool, instrument_id, tables: &[&SidecarBulkRows]) -> AppResult<RefreshSummary>`
(one transaction per instrument — a failure on one name must not roll back the rest);
`refresh` (single) becomes fetch + `apply_tables`. `refresh_view`:
members from `views::view_instruments` (already excludes retired/under-review);
those without `security` → `skipped` + `ingest_issue (instrument_id, 'warn', 'corp_actions_skipped')`;
the rest chunked by 100 → one `fetcher.corp_actions(chunk)` each;
returned tables grouped by `security` string → mapped back to instrument_id (the chunk's own map);
per instrument, `apply_tables` with that instrument's tables; summaries summed.
A security present in the request but absent from every returned table simply contributes nothing (the per-field empty guard already protects its history).

- [x] Failing integration tests (tests/corp_actions.rs):
  1. `a_view_refresh_covers_every_member_in_one_batched_call` — two instruments in a view, mock returns factor tables for both securities; `refresh_view` → `instruments == 2`, `inserted == 2`, `mock.call_count() == 1` (ONE batched call), both instruments have a `corp_action` row.
  2. `a_member_without_a_security_is_skipped_and_reported` — second member's alias closed before `as_of`; summary `skipped == 1`, `instruments == 1`, issue `corp_actions_skipped` exists, and the mock request did NOT include a dead string (assert via calls log content).
  3. `a_view_refresh_is_idempotent` — run twice, second is all `unchanged`, row counts stable.
- [x] Implement; suites green. Commit `feat: whole-view corporate-actions refresh, batched 100 securities per call`.

### Task 3: View-level command + UI

**Files:** `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`, `src/lib/api.ts`, `src/lib/ViewsScreen.svelte`.

- [x] Command `refresh_view_corp_actions(view_id) -> ViewRefreshSummary` (BlpapiMasterFetcher, `as_of = today`); register.
- [x] `api.refreshViewCorpActions(viewId)`; ViewsScreen: per view row, a button `Corp actions (2×N hits)` where N = that view's member count (from `estimates`? No — add member count by calling `api.getViewInstruments(v.id)` on demand is heavy; simplest: label `Refresh corp actions` with title tooltip `2 hits per instrument`), busy state per view, summary/notice line `X instruments: a new, b amended, c withdrawn, d unchanged, e unparsed, f skipped`. Errors to the existing error paragraph.
- [x] `svelte-check` clean; commit `feat(ui): per-view corporate-actions refresh`.

### Task 4: Data tab backend — read commands + CSV export

**Files:** `src-tauri/src/views.rs` or new `src-tauri/src/dataview.rs` (new module preferred), `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`, `src-tauri/tests/dataview.rs`.

**Interfaces (module `dataview`):**
```rust
#[derive(Serialize, sqlx::FromRow)] pub struct ObsRow {
    pub id: i64, pub obs_date: NaiveDate,
    pub value_num: Option<f64>, pub value_text: Option<String>,
    pub basis_note: Option<String>, pub layer: String,
    pub run_id: i64, pub system_from: DateTime<Utc>,
    pub current: bool,
}
pub async fn observations(pool, instrument_id, field_id, include_superseded: bool,
                          limit: i64) -> AppResult<Vec<ObsRow>>;   // ORDER BY obs_date DESC, system_from DESC; LIMIT clamp 1..=5000
#[derive(Serialize, sqlx::FromRow)] pub struct CorpActionFull { /* ActionRow columns + natural_key + payload: serde_json::Value + system_from + current */ }
pub async fn corp_actions_full(pool, instrument_id, include_superseded: bool)
    -> AppResult<Vec<CorpActionFull>>;
pub async fn export_observations_csv(pool, instrument_id, field_id, path: &Path)
    -> AppResult<u64>;   // returns rows written; current rows only; columns: obs_date,value,basis,run_id,recorded_at
pub async fn export_corp_actions_csv(pool, instrument_id, path: &Path) -> AppResult<u64>;
    // current rows; columns: source_field,event_date,amount,operator,flag,dvd_type,frequency,declared_date,record_date,pay_date,amount_status,natural_key,payload_json
```
CSV writing: plain `std::fs::File` + manual escaping (quote fields containing `",\n`; double inner quotes) — no new dependency.

- [x] Failing integration tests (tests/dataview.rs, reuse pipeline-style scaffold): supersede a value then assert `observations(include_superseded=false)` returns 1 current row with `basis_note` starting `RAW`, `=true` returns both ordered current-first per date; `corp_actions_full` carries the payload verbatim; CSV export writes a file whose line count = rows+1 and whose header matches, values quoted correctly (payload contains commas).
- [x] Implement; suites green. Commit `feat: dataview read commands + CSV export`.

### Task 5: Data tab UI

**Files:** `src/routes/+page.svelte` (add tab), `src/lib/DataScreen.svelte` (new), `src/lib/api.ts`.

- [x] Tabs become `Run / Book / Review / Views / Data / Settings` (`+page.svelte`).
- [x] DataScreen: instrument selector (dropdown over `api.listBook()`, label + security), field selector (fields of that instrument's class via `api.listFields()` filtered client-side), `include superseded` checkbox, limit input (default 500). Observations table: date, value, basis, run, recorded-at, `superseded` styling on non-current rows. Corporate-actions table: the ActionRow columns + a per-row `payload` expander (`<details><pre>{JSON.stringify(payload, null, 1)}</pre></details>`). Two export rows following BookScreen's pattern: seeded path `{data_dir}\obs_<instrument>_<field>.csv` / `{data_dir}\corp_actions_<instrument>.csv`, text input, Export button, `Written to …` notice. A visible reminder line: "Read-only: this screen never calls Bloomberg."
- [x] `api.ts`: `listObservations`, `listCorpActionsFull`, `exportObservationsCsv`, `exportCorpActionsCsv` + types.
- [x] `svelte-check` clean; `cargo test` green; commit `feat(ui): Data tab -- stored observations, corp-action payloads, CSV export`.

### Task 6: Full verification + docs touch

- [x] All suites: unit + every `--ignored` suite except the live smoke.
- [x] Append a paragraph to the P3 design doc §2: view-level refresh shipped 2026-08-21 (batched, 2 hits/instrument, skips members without a current security).
- [x] Commit `docs: record the view-level corp-actions refresh in the P3 design`.
