# Pipeline Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the operational gaps found in the 2026-08-20 project review: stale docs, the dead `bulk_rows` wire, field-blind gap detection, whole-view backfill amplification, permanent phantom holiday gaps, dead tickers that are never re-resolved, and the never-booted `bloomdata` database.

**Architecture:** Every change extends an existing seam — `SidecarResponse` gains the field the sidecar already emits, `detect_gaps` gains field-completeness and a non-trading exclusion, `run_backfill` gains an instrument filter, and a new `engine::auto_reresolve_invalid` runs after live runs only (mock-driven tests call it directly). One additive migration (`0002`). No hard hit-cap anywhere (user decision 2026-08-20: keep the soft limit).

**Tech Stack:** Rust (sqlx/Postgres, Tauri), Svelte 5 runes, Python BLPAPI sidecar. Tests: `cargo test` (unit) and `cargo test -- --ignored` (needs local Postgres `bloom_test`).

**Spec:** the review findings in this plan's task intros; P0 facts in `docs/superpowers/specs/2026-08-19-blpapi-field-facts.md`; P1 design in `docs/superpowers/specs/2026-08-19-security-master-design.md`.

## Global Constraints

- Never edit `migrations/0001_init.sql` — it is checksummed by sqlx and already applied to `bloom_test`. New DDL goes in new numbered files.
- Observations, aliases, attrs stay append-only; the only permitted UPDATE is closing `system_to` / `valid_to`.
- Every Bloomberg mnemonic used must be P0-confirmed (`2026-08-19-blpapi-field-facts.md`).
- Hit accounting: one hit per security-field pair; charged at the wire seam (`BlpapiMasterFetcher`), never at call sites.
- No hard 500k cap (explicit user decision, 2026-08-20).
- Integration tests: `#[ignore = "requires postgres"]`, shared `bloom_test`, `common::uniq()` for every UNIQUE-constrained value, no cleanup.
- Run integration tests as: `cd src-tauri && cargo test --test <file> -- --ignored` (default URL `postgres://postgres:postgres@localhost/bloom_test`).

---

### Task 1: Doc alignment and cruft removal

**Files:**
- Modify: `docs/superpowers/specs/2026-08-19-security-master-design.md` (status line; §4.2 attribute paragraph; §5.1; §7 table)
- Modify: `docs/superpowers/specs/smoke-test-checklist.md` (prepend SUPERSEDED banner)
- Delete: the empty root directory `C:UsersLaurentDesktopCCgetbloomdatasrc-taurisrcresolution` (path-mangling artifact, confirmed empty)

**Interfaces:** none (docs only).

- [x] **Step 1: Update the design spec status line**

Replace line 4:
```markdown
**Status:** IMPLEMENTED (P1, this branch) — §4.2, §5.1 and §7 corrected 2026-08-20 to match the shipped code; the authoritative verification record is `docs/superpowers/plans/2026-08-19-p1-smoke-checklist.md`.
```

- [x] **Step 2: Correct §4.2's attribute-source paragraph**

Replace the paragraph beginning "Attribute values are sourced from the P0-verified fields:" with:
```markdown
Attribute values written in P1 come from the six identity-block fields the
code actually fetches (`master_fetch::IDENTITY_FIELDS` →
`engine::attr_pairs`): `NAME` → name, `EXCH_CODE` → exchange,
`CNTRY_ISSUE_ISO` → country, `CRNCY` → currency, `SECURITY_TYP2` →
instrument_type, `MARKET_SECTOR_DES` → asset_class. Validity dates come from
`LISTING_DATE` and `INACTIVE_DATE`. `issuer`, `share_class` and
`fund_vehicle` remain in the CHECK domain but are **not fetched in P1** —
`ID_BB_COMPANY`, `FUND_SHR_CLASS_DESG`, `SHARE_CLASS_TYPE` and `FUND_TYP`
are P5 work (the live smoke pass caught this drift; see the P1 checklist).
```
Keep the existing `status` paragraph unchanged.

- [x] **Step 3: Rewrite §5.1 to match the shipped behaviour**

Replace the first paragraph of §5.1 ("On first resolution, ... passed.") with:
```markdown
Identifier history is an **explicit user action**, never automatic. The
design originally issued `HISTORICAL_IDS_TIME_RANGE` on first resolution,
anchored on the resolved security; P0 §6.5 measured why that is unsafe:
`HISTORICAL_STARTING_IDENTIFIER` names the identifier the chain *started*
from, and resolution only knows the one it *ended* at — anchored on
`META US Equity` the response described the Roundhill Ball Metaverse ETF.
The call now lives in the instrument detail panel
(`commands::ingest_identifier_history`) with a **user-supplied anchor** and
range start, costing 1 call when asked for. Returned rows become
`instrument_alias` rows with `valid_from`/`valid_to` from the `Date` column,
`bbg_action_id` from `Action ID`, and `anchoring_identifier` set to the
anchor that was passed.
```
The FB/META/METV auto-merge-refusal paragraph stays as is.

- [x] **Step 4: Correct the §7 hit table**

Replace the two "resolving a never-seen instrument" rows and add a history row so the table reads:
```markdown
| resolving a never-seen instrument, **unambiguously** | 1 `ReferenceDataRequest` |
| resolving a never-seen instrument **that needs the search** | 3: the identity probe, then `instrumentListRequest`, then a second `ReferenceDataRequest` to resolve the winner |
| identifier history (user-initiated, instrument detail panel) | 1 `HISTORICAL_IDS_TIME_RANGE`, with a user-supplied anchor |
```
After the table, append one sentence:
```markdown
The ledger's *hits* and this table's *calls* are different units: an identity
request is charged securities × 12 `IDENTITY_FIELDS`, matching
`budget::estimate_eod_hits`' security-field accounting.
```

- [x] **Step 5: Mark the pre-P1 checklist superseded**

Prepend to `docs/superpowers/specs/smoke-test-checklist.md`:
```markdown
> **SUPERSEDED (2026-08-20).** This is the pre-P1 draft; it still references
> the deleted `asset` table, TimescaleDB, and the removed Assets screen.
> The live, executed record is
> `docs/superpowers/plans/2026-08-19-p1-smoke-checklist.md`.

```

- [x] **Step 6: Remove the junk directory and commit**

```bash
rmdir "C:UsersLaurentDesktopCCgetbloomdatasrc-taurisrcresolution"
git add -A docs/
git commit -m "docs: align the P1 design spec with the shipped code, supersede the pre-P1 checklist"
```

---

### Task 2: Carry `bulk_rows` through `SidecarResponse`

The sidecar computes and emits `bulk_rows` (`blp_fetch.py` `emit()`); Rust's `SidecarResponse` has no such field, so serde drops it silently. P3 consumes it; this task just stops the drop.

**Files:**
- Modify: `src-tauri/src/fetch.rs` (types + tests)

**Interfaces:**
- Produces: `pub struct SidecarBulkRows { pub security: String, pub field: String, pub rows: Vec<serde_json::Map<String, serde_json::Value>> }` and `SidecarResponse.bulk_rows: Vec<SidecarBulkRows>` — P3's `MasterFetcher::corp_actions` deserializes this shape.

- [x] **Step 1: Write the failing test** (in `fetch.rs` `mod tests`, after `text_field_accepts_a_number_by_rendering_it`)

```rust
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
```

- [x] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo test --lib fetch::tests::sidecar_bulk_rows_are_carried_not_dropped`
Expected: FAIL — `no field 'bulk_rows' on type SidecarResponse` (compile error).

- [x] **Step 3: Add the types** (in `fetch.rs`, next to `SidecarProblem`)

```rust
/// One security × one bulk (table-valued) field, rows verbatim from the
/// sidecar's `parse_bulk_message`. Column names are Bloomberg's own, spaces
/// and all; nothing here interprets them (P3's corp-action ingester does).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SidecarBulkRows {
    pub security: String,
    pub field: String,
    pub rows: Vec<serde_json::Map<String, serde_json::Value>>,
}
```
and in `SidecarResponse`:
```rust
    #[serde(default)]
    pub bulk_rows: Vec<SidecarBulkRows>,
```

- [x] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo test --lib fetch::`
Expected: all fetch tests PASS.

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/fetch.rs
git commit -m "fix: carry the sidecar's bulk_rows through SidecarResponse instead of dropping them"
```

---

### Task 3: Migration 0002 — `non_trading_day` and `ingest_issue.created_at`

**Files:**
- Create: `src-tauri/migrations/0002_non_trading_and_issue_time.sql`
- Test: `src-tauri/tests/schema.rs` (append)

**Interfaces:**
- Produces: table `non_trading_day (instrument_id, obs_date, source, recorded_at)` PK `(instrument_id, obs_date)` — consumed by Task 6; column `ingest_issue.created_at TIMESTAMPTZ NOT NULL DEFAULT now()` — consumed by Task 7's cooldown.

- [x] **Step 1: Write the failing test** (append to `src-tauri/tests/schema.rs`)

```rust
/// Task 6 records evidence-based non-trading days; the PK is the dedup.
/// Task 7's auto-re-resolve cooldown needs to know WHEN an issue was written.
#[tokio::test]
#[ignore = "requires postgres"]
async fn non_trading_day_dedups_and_issues_are_timestamped() {
    let pool = common::pool().await;
    let inst = getbloomdata_lib::instrument::store::create(&pool).await.unwrap();
    let d: chrono::NaiveDate = "2026-08-14".parse().unwrap();
    sqlx::query("INSERT INTO non_trading_day (instrument_id, obs_date) VALUES ($1,$2)")
        .bind(inst.instrument_id).bind(d).execute(&pool).await.unwrap();
    let dup = sqlx::query("INSERT INTO non_trading_day (instrument_id, obs_date) VALUES ($1,$2)")
        .bind(inst.instrument_id).bind(d).execute(&pool).await;
    assert!(dup.is_err(), "the (instrument, date) PK must refuse a duplicate");

    sqlx::query("INSERT INTO ingest_issue (instrument_id, severity, code) VALUES ($1,'warn','x')")
        .bind(inst.instrument_id).execute(&pool).await.unwrap();
    let ts: chrono::DateTime<chrono::Utc> = sqlx::query_scalar(
        "SELECT created_at FROM ingest_issue WHERE instrument_id = $1 ORDER BY id DESC LIMIT 1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert!(ts <= chrono::Utc::now());
}
```

- [x] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo test --test schema -- --ignored non_trading_day_dedups`
Expected: FAIL — `relation "non_trading_day" does not exist`.

- [x] **Step 3: Write the migration**

`src-tauri/migrations/0002_non_trading_and_issue_time.sql`:
```sql
-- Evidence-based non-trading days. No external holiday calendar exists in
-- this system (design decision, 2026-08-13 spec §5.2); instead, a day is
-- recorded here when Bloomberg itself answered "no trading session" --
-- either a dated no_data on a single-day run, or a silent omission inside a
-- multi-day ACTIVE_DAYS_ONLY range that returned neighbours. detect_gaps
-- treats these dates as covered, which is what stops a holiday from being a
-- permanent, un-backfillable phantom gap.
CREATE TABLE non_trading_day (
  instrument_id BIGINT NOT NULL REFERENCES instrument(instrument_id),
  obs_date      DATE NOT NULL,
  source        TEXT NOT NULL DEFAULT 'no_data'
                CHECK (source IN ('no_data','range_inference')),
  recorded_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (instrument_id, obs_date)
);

-- When was this issue raised? Needed by the auto-re-resolve cooldown (an
-- instrument is probed at most once per cooldown window), and honest for the
-- UI regardless.
ALTER TABLE ingest_issue
  ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
```

- [x] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo test --test schema -- --ignored`
Expected: all PASS (migration applies on `common::pool()`).

- [x] **Step 5: Commit**

```bash
git add src-tauri/migrations/0002_non_trading_and_issue_time.sql src-tauri/tests/schema.rs
git commit -m "feat(db): non_trading_day table and ingest_issue.created_at (migration 0002)"
```

---

### Task 4: Field-complete gap detection

`detect_gaps` currently counts a date covered if the instrument has ANY current observation on it (`scheduler.rs:236-241`). A `PX_LAST` hole behind a successful `PX_VOLUME` is invisible. New rule: a date is covered for an instrument only when **every non-text field the view configures for that instrument's asset class** has a current raw EOD observation. Text fields are excluded because backfill cannot recover them by design (`fetch.rs` `plan_requests`) — a gap that cannot be fixed is noise.

**Files:**
- Modify: `src-tauri/src/scheduler.rs` (`detect_gaps`)
- Modify: `src-tauri/tests/pipeline.rs` (`scaffold` gains a `view_field` row; new test)

**Interfaces:**
- Consumes: `views::view_fields(pool, view_id) -> Vec<fields::FieldDef>` (has `.id`, `.asset_class_id`, `.value_kind`).
- Produces: `detect_gaps` signature unchanged; `Gap` struct unchanged (Task 6 modifies the body again to exclude non-trading days).

- [x] **Step 1: Make `scaffold` configure the view's fields** (in `tests/pipeline.rs`, after the `view_instrument` insert)

```rust
    sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
        .bind(vid).bind(fid).execute(pool).await.unwrap();
```
(Check the actual `view_field` column names in `migrations/0001_init.sql:315` first and match them.)

- [x] **Step 2: Write the failing test** (append to `tests/pipeline.rs`)

```rust
/// Gap detection is per (instrument, field-complete date): a PX_LAST hole
/// must not hide behind a PX_VOLUME that succeeded. Text fields do not
/// count -- backfill cannot recover them by design, and an unfixable gap is
/// noise that buries the fixable ones.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_missing_field_behind_a_present_sibling_is_still_a_gap() {
    let pool = common::pool().await;
    let (iid, px_last, vid, rid) = scaffold(&pool, "GAPFLD").await;
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    let px_volume: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_VOLUME','Volume','numeric') RETURNING id")
        .bind(class).fetch_one(&pool).await.unwrap();
    let name_fld: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'NAME','Name','text') RETURNING id")
        .bind(class).fetch_one(&pool).await.unwrap();
    for f in [px_volume, name_fld] {
        sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
            .bind(vid).bind(f).execute(&pool).await.unwrap();
    }

    // Tuesday 2026-08-18: PX_LAST present, PX_VOLUME missing, NAME missing.
    let day = d("2026-08-18");
    sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,$3,'raw',1,100.0,$4)")
        .bind(iid).bind(px_last).bind(day).bind(rid)
        .execute(&pool).await.unwrap();

    let today = d("2026-08-19");
    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 1, today).await.unwrap();
    assert!(gaps.iter().any(|g| g.instrument_id == iid && g.start == day),
            "PX_VOLUME missing on {day} must surface as a gap: {gaps:?}");

    // Fill PX_VOLUME; NAME (text) stays absent -- and must not keep the gap open.
    sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,$3,'raw',1,5.0e6,$4)")
        .bind(iid).bind(px_volume).bind(day).bind(rid)
        .execute(&pool).await.unwrap();
    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 1, today).await.unwrap();
    assert!(gaps.iter().all(|g| g.instrument_id != iid),
            "both numeric fields present; a missing TEXT field is not a gap: {gaps:?}");
}
```

- [x] **Step 3: Run to verify it fails**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored a_missing_field_behind_a_present_sibling_is_still_a_gap`
Expected: FAIL on the first assertion (old query sees ANY observation as coverage).

- [x] **Step 4: Rewrite `detect_gaps`** (replace the query and per-member loop in `scheduler.rs`; keep the doc comment, extend it with the field-completeness rule)

```rust
pub async fn detect_gaps(pool: &PgPool, view_id: i64, lookback_days: i64,
                         today: NaiveDate) -> AppResult<Vec<Gap>> {
    let start = today - Duration::days(lookback_days);
    let end = today - Duration::days(1);
    let members = crate::views::view_instruments(pool, view_id).await?;
    if members.is_empty() {
        return Ok(Vec::new());
    }
    // A date is covered only when EVERY non-text field the view configures
    // for the member's class has a current raw EOD row. Text fields are
    // excluded: backfill cannot recover them by design (plan_requests), and
    // an unfixable gap is noise that buries the fixable ones.
    let fields = crate::views::view_fields(pool, view_id).await?;
    let mut expected: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for f in fields.iter().filter(|f| f.value_kind != "text") {
        *expected.entry(f.asset_class_id).or_insert(0) += 1;
    }

    let rows: Vec<(i64, NaiveDate, i64)> = sqlx::query_as(
        "SELECT o.instrument_id, o.obs_date, count(DISTINCT o.field_id)::bigint
           FROM observation o
           JOIN view_instrument vi ON vi.instrument_id = o.instrument_id
                                  AND vi.view_id = $1
           JOIN view_field vf ON vf.view_id = vi.view_id AND vf.field_id = o.field_id
           JOIN field_def fd ON fd.id = o.field_id AND fd.value_kind <> 'text'
          WHERE o.obs_date BETWEEN $2 AND $3
            AND o.system_to = 'infinity'
            AND o.layer = 'raw' AND o.granularity = 'eod'
          GROUP BY o.instrument_id, o.obs_date")
        .bind(view_id).bind(start).bind(end).fetch_all(pool).await?;

    let mut out = Vec::new();
    for m in members {
        let Some(&need) = expected.get(&m.asset_class_id) else {
            // The view fetches nothing history-shaped for this class, so no
            // date can be missing anything backfill could supply.
            continue;
        };
        let present: HashSet<NaiveDate> = rows.iter()
            .filter(|(iid, _, have)| *iid == m.instrument_id && *have >= need)
            .map(|(_, d, _)| *d)
            .collect();
        for (s, e) in group_ranges(&missing_weekdays(&present, start, end),
                                   orchestrator::BACKFILL_CAP_DAYS) {
            out.push(Gap { instrument_id: m.instrument_id, label: m.label.clone(),
                           start: s, end: e });
        }
    }
    Ok(out)
}
```

- [x] **Step 5: Run the new test and the two existing gap tests**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored`
Expected: all PASS, including `a_gap_in_one_instrument_is_not_hidden_by_another_that_reported` and `a_retired_member_is_not_reported_as_a_gap` (they now rely on Step 1's `view_field` row).

- [x] **Step 6: Commit**

```bash
git add src-tauri/src/scheduler.rs src-tauri/tests/pipeline.rs
git commit -m "fix: gap detection requires every backfillable field, not any observation"
```

---

### Task 5: Per-instrument backfill

A one-instrument gap currently re-fetches the entire view (`commands.rs:353-357` admits it). `run_backfill` gains an optional instrument filter; the Gaps table's Backfill button passes the gap's own instrument.

**Files:**
- Modify: `src-tauri/src/orchestrator.rs` (`load_view` filter param; `run_backfill`/`run_backfill_with` signature; `run_eod_with` call site passes `None`)
- Modify: `src-tauri/src/commands.rs` (`run_backfill_now`)
- Modify: `src/lib/api.ts`, `src/lib/RunScreen.svelte`
- Test: `src-tauri/tests/pipeline.rs`
- Also modify call sites: `src-tauri/tests/db_integration.rs` (any `run_backfill(_with)` calls gain `None`)

**Interfaces:**
- Produces: `pub async fn run_backfill(pool, cfg, view_id, start, end, instrument_ids: Option<&[i64]>, confirmed) -> AppResult<RunOutcome>` (same extra param on `run_backfill_with`); Tauri command `run_backfill_now(view_id, start, end, instrument_ids: Option<Vec<i64>>, confirmed)`; `api.runBackfillNow(viewId, start, end, confirmed, instrumentIds?: number[] | null)`.

- [x] **Step 1: Write the failing test** (append to `tests/pipeline.rs`)

```rust
/// A one-instrument gap must not cost a whole-view refetch. The filter is
/// applied at load_view, so the estimate, the request and the ingest all see
/// only the targeted instrument.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_filtered_backfill_fetches_only_the_target_instrument() {
    struct Recording(std::sync::Mutex<Vec<Vec<i64>>>);
    impl DataFetcher for Recording {
        async fn fetch(&self, req: &FetchRequest, _audit: Option<&Path>)
            -> AppResult<FetchOutcome> {
            self.0.lock().unwrap()
                .push(req.assets.iter().map(|a| a.instrument_id).collect());
            Ok(FetchOutcome::default())
        }
    }

    let pool = common::pool().await;
    let (a, _fid, vid, _rid) = scaffold(&pool, "BFONE").await;
    // Second member, same class.
    let class: i64 = sqlx::query_scalar(
        "SELECT asset_class_id FROM book_entry WHERE instrument_id = $1")
        .bind(a).fetch_one(&pool).await.unwrap();
    let b_inst = store::create(&pool).await.unwrap();
    let b = b_inst.instrument_id;
    let b_sec = format!("{} US Equity", uniq("BFTWO"));
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, b, &NewAlias {
        id_type: "bdp_security".into(), value: b_sec.clone(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(b).bind(class).bind(&b_sec).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(vid).bind(b).execute(&pool).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = PipelineConfig {
        data_dir: dir.path().to_path_buf(),
        python_path: "python".into(), script_path: "scripts/blp_fetch.py".into(),
        request_timeout_s: 5, soft_limit: 100_000,
    };
    let rec = Recording(std::sync::Mutex::new(Vec::new()));
    let out = orchestrator::run_backfill_with(
        &pool, &cfg, &rec, vid, d("2026-08-17"), d("2026-08-18"),
        Some(&[b]), true).await.unwrap();
    assert!(matches!(out, RunOutcome::Completed { .. }));
    let seen = rec.0.lock().unwrap().clone();
    assert_eq!(seen, vec![vec![b]],
               "only the targeted instrument may reach the fetcher: {seen:?}");
}
```
(`tempfile` is already a dev-dependency — `db_integration.rs` uses it; if the import is missing in pipeline.rs, add `tempfile` usage exactly as db_integration does.)

- [x] **Step 2: Run to verify it fails**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored a_filtered_backfill_fetches_only_the_target_instrument`
Expected: FAIL — compile error, `run_backfill_with` takes no `instrument_ids`.

- [x] **Step 3: Implement the filter**

`orchestrator.rs`:
```rust
async fn load_view(pool: &PgPool, view_id: i64, only: Option<&[i64]>) -> AppResult<Loaded> {
    ...
    let mut members = views::view_instruments(pool, view_id).await?;
    if let Some(ids) = only {
        // A filtered backfill (a per-instrument gap) fetches only its target;
        // an id not in the view is simply absent, and plan_requests' empty-
        // assets validation reports the net result.
        members.retain(|m| ids.contains(&m.instrument_id));
    }
    ...
}
```
`run_eod_with`: `load_view(pool, view_id, None)`.
`run_backfill` / `run_backfill_with`: add `instrument_ids: Option<&[i64]>` between `end` and `confirmed`; pass through to `load_view(pool, view_id, instrument_ids)`. Estimation code is unchanged — it runs on the already-filtered `loaded.assets`.

`commands.rs`:
```rust
#[tauri::command]
pub async fn run_backfill_now(state: State<'_, AppState>, view_id: i64,
                              start: String, end: String,
                              instrument_ids: Option<Vec<i64>>, confirmed: bool)
    -> Result<RunOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let s = start.parse().map_err(|_| AppError::Validation("bad start date".into()))?;
    let e = end.parse().map_err(|_| AppError::Validation("bad end date".into()))?;
    orchestrator::run_backfill(&state.pool, &cfg, view_id, s, e,
                               instrument_ids.as_deref(), confirmed).await
}
```
Update the `GapRow` doc comment in `commands.rs` (the "Backfilling still runs the whole view" sentence is now false — say the button backfills only the gap's instrument).

Fix compile errors at existing call sites (`tests/db_integration.rs` backfill tests, if any, gain `None`).

- [x] **Step 4: Run to verify it passes**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored && cargo test`
Expected: PASS.

- [x] **Step 5: Wire the UI**

`src/lib/api.ts`:
```ts
  runBackfillNow: (viewId: number, start: string, end: string, confirmed: boolean,
                   instrumentIds: number[] | null = null) =>
    invoke<RunOutcome>("run_backfill_now", { viewId, start, end, instrumentIds, confirmed }),
```
`src/lib/RunScreen.svelte`:
- `PendingConfirm` backfill variant gains `instrument_ids: number[] | null`.
- `backfillRange` becomes `backfillGap(g: GapRow)` calling
  `api.runBackfillNow(selectedViewId, g.start, g.end, false, [g.instrument_id])`,
  storing `instrument_ids: [g.instrument_id]` in `pending` when confirmation is required.
- `confirmPending` passes `pending.instrument_ids` through.
- Gap button: `onclick={() => backfillGap(g)}`.
- Update the explanatory copy: `Per instrument: the Backfill button fetches only the instrument shown, for the range shown.`

Run: `npx svelte-check --threshold error` (or `npm run check` if defined) — expect no new errors.

- [x] **Step 6: Commit**

```bash
git add src-tauri/src/orchestrator.rs src-tauri/src/commands.rs src-tauri/tests/ src/lib/api.ts src/lib/RunScreen.svelte
git commit -m "feat: per-instrument backfill -- a one-name gap no longer refetches the whole view"
```

---

### Task 6: Record non-trading days; exclude them from gaps

Holidays currently surface as permanent, un-clearable gaps: a holiday run stores nothing, `detect_gaps` lists the date forever, and backfilling it can never produce a row (`ACTIVE_DAYS_ONLY`). Fix with evidence, not a calendar: record `(instrument, date)` as non-trading when Bloomberg itself said so.

**Files:**
- Modify: `src-tauri/src/ingest.rs` (new `record_non_trading_days`)
- Modify: `src-tauri/src/orchestrator.rs` (`execute` calls it after ingest)
- Modify: `src-tauri/src/scheduler.rs` (`detect_gaps` unions non-trading dates into `present`)
- Test: `src-tauri/tests/pipeline.rs`

**Interfaces:**
- Consumes: `non_trading_day` table (Task 3); `FetchRequest` (`.assets`, `.start`, `.end`, `.is_single_day()`); `FetchOutcome` (`.cells`, `.problems`).
- Produces: `pub async fn record_non_trading_days(pool: &PgPool, req: &FetchRequest, outcome: &FetchOutcome) -> AppResult<u64>` (rows inserted).

- [x] **Step 1: Write the failing tests** (append to `tests/pipeline.rs`)

```rust
/// Rule A: a dated no_data with no cells and no other problem for that
/// (instrument, day) is Bloomberg saying "no trading session". Recording it
/// is what lets detect_gaps stop reporting the holiday forever.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_dated_no_data_marks_a_non_trading_day_and_clears_the_gap() {
    let pool = common::pool().await;
    let (iid, fid, vid, rid) = scaffold(&pool, "NTD1").await;
    let holiday = d("2026-08-18");
    let req = FetchRequest { run_id: rid, assets: vec![], fields: vec![],
                             start: holiday, end: holiday };
    let out = FetchOutcome {
        cells: vec![],
        problems: vec![getbloomdata_lib::fetch::CellProblem {
            instrument_id: Some(iid), field_id: Some(fid),
            obs_date: Some(holiday), code: "no_data".into(),
            detail: "no trading day returned".into(),
        }],
    };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 1);

    let gaps = getbloomdata_lib::scheduler::detect_gaps(&pool, vid, 1, d("2026-08-19"))
        .await.unwrap();
    assert!(gaps.iter().all(|g| g.instrument_id != iid),
            "a recorded non-trading day is not a gap: {gaps:?}");
}

/// An invalid_security day is NOT a holiday -- nothing may be recorded.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_invalid_security_day_is_not_recorded_as_non_trading() {
    let pool = common::pool().await;
    let (iid, fid, _vid, rid) = scaffold(&pool, "NTD2").await;
    let day = d("2026-08-18");
    let req = FetchRequest { run_id: rid, assets: vec![], fields: vec![],
                             start: day, end: day };
    let out = FetchOutcome {
        cells: vec![],
        problems: vec![
            getbloomdata_lib::fetch::CellProblem {
                instrument_id: Some(iid), field_id: Some(fid),
                obs_date: Some(day), code: "no_data".into(), detail: String::new() },
            getbloomdata_lib::fetch::CellProblem {
                instrument_id: Some(iid), field_id: None,
                obs_date: Some(day), code: "invalid_security".into(), detail: String::new() },
        ],
    };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 0, "mixed signals are not holiday evidence");
}

/// Rule B: inside a multi-day range, ACTIVE_DAYS_ONLY omits a non-trading
/// day silently. A weekday with no cells, for an instrument that DID return
/// cells on other days of the range, is non-trading by inference.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_silent_weekday_inside_a_range_with_neighbours_is_non_trading() {
    let pool = common::pool().await;
    let (iid, fid, _vid, rid) = scaffold(&pool, "NTD3").await;
    let (mon, tue, wed) = (d("2026-08-17"), d("2026-08-18"), d("2026-08-19"));
    let req = FetchRequest { run_id: rid, assets: vec![], fields: vec![],
                             start: mon, end: wed };
    let cell = |dt| ObsCell { instrument_id: iid, field_id: fid, obs_date: dt,
                              value: CellValue::Num(1.0) };
    let out = FetchOutcome { cells: vec![cell(mon), cell(wed)], problems: vec![] };
    let n = ingest::record_non_trading_days(&pool, &req, &out).await.unwrap();
    assert_eq!(n, 1);
    let src: String = sqlx::query_scalar(
        "SELECT source FROM non_trading_day WHERE instrument_id=$1 AND obs_date=$2")
        .bind(iid).bind(tue).fetch_one(&pool).await.unwrap();
    assert_eq!(src, "range_inference");
}
```

- [x] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored non_trading`
Expected: FAIL — `record_non_trading_days` does not exist.

- [x] **Step 3: Implement** (append to `ingest.rs`)

```rust
use chrono::NaiveDate;
use std::collections::{HashMap, HashSet};

/// Evidence-based non-trading days -- no external holiday calendar exists in
/// this system, so a day is recorded only when Bloomberg itself said there
/// was no session:
/// - rule A: a (instrument, day) with zero cells, >=1 dated `no_data`, and
///   no other-coded dated problem;
/// - rule B (multi-day ranges): ACTIVE_DAYS_ONLY omits non-trading days
///   silently, so a weekday with zero cells and zero dated problems, for an
///   instrument that returned cells elsewhere in the range, is non-trading
///   by inference (this also covers per-security suspensions, which equally
///   have no price to backfill).
pub async fn record_non_trading_days(pool: &PgPool, req: &crate::fetch::FetchRequest,
                                     outcome: &FetchOutcome) -> AppResult<u64> {
    let mut cells: HashMap<i64, HashSet<NaiveDate>> = HashMap::new();
    for c in &outcome.cells {
        cells.entry(c.instrument_id).or_default().insert(c.obs_date);
    }
    let mut no_data: HashSet<(i64, NaiveDate)> = HashSet::new();
    let mut other: HashSet<(i64, NaiveDate)> = HashSet::new();
    for p in &outcome.problems {
        if let (Some(iid), Some(d)) = (p.instrument_id, p.obs_date) {
            if p.code == "no_data" { no_data.insert((iid, d)); }
            else { other.insert((iid, d)); }
        }
    }

    let mut marks: Vec<(i64, NaiveDate, &'static str)> = Vec::new();
    for &(iid, d) in &no_data {
        let has_cell = cells.get(&iid).is_some_and(|s| s.contains(&d));
        if !has_cell && !other.contains(&(iid, d)) {
            marks.push((iid, d, "no_data"));
        }
    }
    if req.start < req.end {
        for (&iid, have) in &cells {
            let mut day = req.start;
            while day <= req.end {
                if !crate::scheduler::is_weekend(day)
                    && !have.contains(&day)
                    && !no_data.contains(&(iid, day))
                    && !other.contains(&(iid, day)) {
                    marks.push((iid, day, "range_inference"));
                }
                day += chrono::Duration::days(1);
            }
        }
    }

    let mut inserted = 0u64;
    for (iid, d, src) in marks {
        let r = sqlx::query(
            "INSERT INTO non_trading_day (instrument_id, obs_date, source)
             VALUES ($1,$2,$3) ON CONFLICT DO NOTHING")
            .bind(iid).bind(d).bind(src).execute(pool).await?;
        inserted += r.rows_affected();
    }
    Ok(inserted)
}
```

Hook into `orchestrator::execute`, right after the successful `ingest_outcome` (before the final status UPDATE):
```rust
    // Advisory, like the hit ledger: losing a holiday mark must not fail a
    // run that already ingested its data.
    if let Err(e) = ingest::record_non_trading_days(pool, &req, &outcome).await {
        eprintln!("warning: non-trading-day recording failed for run {run_id}: {e}");
    }
```

`scheduler::detect_gaps` — after building the per-member `present` set (Task 4's shape), union in the non-trading dates. Add one query before the member loop:
```rust
    let non_trading: Vec<(i64, NaiveDate)> = sqlx::query_as(
        "SELECT n.instrument_id, n.obs_date
           FROM non_trading_day n
           JOIN view_instrument vi ON vi.instrument_id = n.instrument_id
          WHERE vi.view_id = $1 AND n.obs_date BETWEEN $2 AND $3")
        .bind(view_id).bind(start).bind(end).fetch_all(pool).await?;
```
and inside the loop:
```rust
        let mut present: HashSet<NaiveDate> = /* Task 4's filter/collect */;
        present.extend(non_trading.iter()
            .filter(|(iid, _)| *iid == m.instrument_id)
            .map(|(_, d)| *d));
```

- [x] **Step 4: Run to verify they pass**

Run: `cd src-tauri && cargo test --test pipeline -- --ignored && cargo test`
Expected: PASS.

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/ingest.rs src-tauri/src/orchestrator.rs src-tauri/src/scheduler.rs src-tauri/tests/pipeline.rs
git commit -m "feat: record evidence-based non-trading days and stop reporting them as gaps"
```

---

### Task 7: Auto re-resolution of dead securities, by FIGI

A renamed instrument keeps sending its dead ticker every day until a human notices `invalid_security` issues. After a **live** run, for each instrument that came back `invalid_security`, probe Bloomberg once by its FIGI (`/bbgid/<figi>` — the parsekeyable form; FIGI is the stable identifier and needs no yellow key) and reconcile the answer through the exact path a manual re-resolution uses. Cooldown: at most one probe per instrument per 7 days (12 hits each). Wired only in the live wrappers (`run_eod`, `run_backfill`) so every existing mock-fetcher test is untouched; tests drive the new function directly with `MockMasterFetcher`.

**Files:**
- Modify: `src-tauri/src/resolution/engine.rs` (new `auto_reresolve_invalid`)
- Modify: `src-tauri/src/orchestrator.rs` (`run_eod`, `run_backfill` call it after `Completed`)
- Test: `src-tauri/tests/resolution.rs`

**Interfaces:**
- Consumes: `MasterFetcher::identity`, `record_decision`, `reconcile_identity` (both private, same module), `ingest_issue.created_at` (Task 3).
- Produces: `pub async fn auto_reresolve_invalid<F: MasterFetcher>(pool: &PgPool, fetcher: &F, run_id: i64, as_of: NaiveDate) -> AppResult<u32>` (instruments re-pointed).

- [x] **Step 1: Write the failing tests** (append to `tests/resolution.rs`; reuse that file's existing helpers for creating an instrument with a FIGI and a `bdp_security` alias — follow the pattern of `re_resolving_the_same_figi_under_a_new_security_records_the_rename`)

```rust
/// A run that saw invalid_security for an instrument probes Bloomberg by the
/// instrument's FIGI and lands the rename through reconcile_identity: the
/// dead period closes at as_of, the new one opens, series stay on the same
/// instrument_id.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_invalid_security_run_triggers_a_figi_probe_that_lands_the_rename() {
    let pool = common::pool().await;
    // Instrument wearing a FIGI and a soon-dead security.
    let figi = uniq("BBGAUTO");
    let old_sec = format!("{} US Equity", uniq("DEADT"));
    let new_sec = format!("{} US Equity", uniq("NEWT"));
    let (iid, run_id) = scaffold_dead_run(&pool, &figi, &old_sec).await;

    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": new_sec, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {"ID_BB_GLOBAL": figi, "NAME": "RENAMED CO",
                          "EXCH_CODE": "US", "CRNCY": "USD",
                          "MARKET_SECTOR_DES": "Equity"}}]}]),
        ..Default::default()
    };
    let as_of = chrono::Local::now().date_naive();
    let n = engine::auto_reresolve_invalid(&pool, &mock, run_id, as_of).await.unwrap();
    assert_eq!(n, 1);
    assert_eq!(mock.call_count(), 1, "exactly one identity probe");

    let secs: Vec<(String, chrono::NaiveDate)> = sqlx::query_as(
        "SELECT value, valid_to FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND system_to = 'infinity' ORDER BY valid_from")
        .bind(iid).fetch_all(&pool).await.unwrap();
    assert_eq!(secs.len(), 2, "rename = two periods: {secs:?}");
    assert_eq!(secs[0].0, old_sec);
    assert_eq!(secs[0].1, as_of, "old period closes at discovery");
    assert_eq!(secs[1].0, new_sec);
}

/// The cooldown: a second run the same week must not spend another 12 hits
/// on the same instrument.
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_probed_instrument_is_not_probed_again_within_the_cooldown() {
    let pool = common::pool().await;
    let figi = uniq("BBGCOOL");
    let old_sec = format!("{} US Equity", uniq("DEADC"));
    let (iid, run_id) = scaffold_dead_run(&pool, &figi, &old_sec).await;
    let mock = MockMasterFetcher::default(); // empty identity answer
    let as_of = chrono::Local::now().date_naive();
    engine::auto_reresolve_invalid(&pool, &mock, run_id, as_of).await.unwrap();
    assert_eq!(mock.call_count(), 1);
    // Same instrument, a later run, same week:
    let run2 = insert_invalid_security_run(&pool, iid).await;
    engine::auto_reresolve_invalid(&pool, &mock, run2, as_of).await.unwrap();
    assert_eq!(mock.call_count(), 1, "cooldown must swallow the second probe");
}

/// No FIGI, no probe -- there is nothing stable to ask Bloomberg about.
#[tokio::test]
#[ignore = "requires postgres"]
async fn an_instrument_without_a_figi_is_skipped_not_probed() {
    let pool = common::pool().await;
    let old_sec = format!("{} US Equity", uniq("NOFIG"));
    let (_iid, run_id) = scaffold_dead_run_no_figi(&pool, &old_sec).await;
    let mock = MockMasterFetcher::default();
    let n = engine::auto_reresolve_invalid(
        &pool, &mock, run_id, chrono::Local::now().date_naive()).await.unwrap();
    assert_eq!(n, 0);
    assert_eq!(mock.call_count(), 0, "no FIGI means no Bloomberg call at all");
}
```

Write the three small helpers (`scaffold_dead_run`, `scaffold_dead_run_no_figi`, `insert_invalid_security_run`) in the same file: create instrument (+ `store::set_bloomberg_ids` for the FIGI variant), insert the `bdp_security` alias, create a view + `run` row (`kind='eod'`, `status='partial'`), and insert `ingest_issue (run_id, instrument_id, severity, code) VALUES ($1,$2,'warn','invalid_security')`. Copy the alias-insertion pattern from `tests/pipeline.rs::scaffold`.

- [x] **Step 2: Run to verify they fail**

Run: `cd src-tauri && cargo test --test resolution -- --ignored auto_reresolve`
Expected: FAIL — `auto_reresolve_invalid` not found.

- [x] **Step 3: Implement** (in `resolution/engine.rs`)

```rust
/// One probe per instrument per this many days: a permanently dead
/// instrument (delisted, no successor) would otherwise cost 12 hits every
/// single day forever.
const AUTO_RERESOLVE_COOLDOWN_DAYS: i32 = 7;

/// After a run, re-point instruments whose security Bloomberg rejected.
///
/// The dead string cannot be resolved -- it is dead. The FIGI can: it is the
/// one identifier a rename never touches. `/bbgid/<figi>` is the
/// parsekeyable form and needs no yellow key. The answer lands through
/// `reconcile_identity`, i.e. exactly the close-and-insert a manual
/// re-resolution performs; nothing here can mint a second instrument.
///
/// Called only from the LIVE wrappers (`orchestrator::run_eod`,
/// `run_backfill`); every outcome, including the skips, is written to
/// `ingest_issue` so the run screen shows what happened and the cooldown has
/// a record to key on. Returns how many instruments were re-pointed.
pub async fn auto_reresolve_invalid<F: MasterFetcher>(
    pool: &PgPool, fetcher: &F, run_id: i64, as_of: NaiveDate) -> AppResult<u32>
{
    let dead: Vec<i64> = sqlx::query_scalar(
        "SELECT DISTINCT instrument_id FROM ingest_issue
          WHERE run_id = $1 AND code = 'invalid_security'
            AND instrument_id IS NOT NULL")
        .bind(run_id).fetch_all(pool).await?;

    let mut repointed = 0u32;
    for iid in dead {
        let probed_recently: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM ingest_issue
              WHERE instrument_id = $1 AND code LIKE 'auto_reresolve%'
                AND created_at > now() - make_interval(days => $2)")
            .bind(iid).bind(AUTO_RERESOLVE_COOLDOWN_DAYS).fetch_one(pool).await?;
        if probed_recently > 0 {
            continue;
        }

        let issue = |code: &'static str, detail: String| async move {
            sqlx::query(
                "INSERT INTO ingest_issue (run_id, instrument_id, severity, code, detail)
                 VALUES ($1,$2,'warn',$3,$4)")
                .bind(run_id).bind(iid).bind(code).bind(detail)
                .execute(pool).await.map(|_| ())
        };

        let figi: Option<String> = sqlx::query_scalar(
            "SELECT id_bb_global FROM instrument WHERE instrument_id = $1")
            .bind(iid).fetch_one(pool).await?;
        let Some(figi) = figi else {
            issue("auto_reresolve_skipped", "no FIGI on record".into()).await?;
            continue;
        };

        let probe = format!("/bbgid/{figi}");
        let answered = match fetcher.identity(&[probe.clone()]).await {
            Ok(a) => a,
            Err(e) => {
                issue("auto_reresolve_failed", format!("identity probe failed: {e}")).await?;
                continue;
            }
        };
        let Some(block) = answered.parsed.first() else {
            issue("auto_reresolve_no_answer",
                  format!("Bloomberg returned nothing for {probe}")).await?;
            continue;
        };
        // The probe asked about OUR figi; an answer wearing another one (or
        // none) must not be reconciled onto this instrument.
        if block.figi.as_deref() != Some(figi.as_str()) {
            issue("auto_reresolve_mismatch",
                  format!("probe {probe} answered with figi {:?}", block.figi)).await?;
            continue;
        }

        let input = ResolveInput {
            raw: probe.clone(), yellow_key: String::new(),
            hints: Hints::default(), as_of, decided_by: "auto".into(),
        };
        let decision_id = record_decision(pool, &input, &probe, "auto_reresolve",
                                          Some(iid), &serde_json::json!([]),
                                          Some(&answered.raw)).await?;
        reconcile_identity(pool, iid, block, decision_id, as_of).await?;
        issue("auto_reresolve", format!("re-pointed to {}", block.security)).await?;
        repointed += 1;
    }
    Ok(repointed)
}
```
(If the `issue` closure fights the borrow checker over `pool`/`run_id`/`iid` captures, inline it as a small `async fn note_issue(pool: &PgPool, run_id: i64, iid: i64, code: &str, detail: &str) -> AppResult<()>` beside `auto_reresolve_invalid` — same statements.)

Hook the live wrappers in `orchestrator.rs` (`run_eod` and `run_backfill`, NOT the `_with` variants):
```rust
    let result = run_eod_with(pool, cfg, &BlpapiFetcher { cfg }, view_id, trigger,
                              obs_date, confirmed).await;
    if let Ok(RunOutcome::Completed { run_id, .. }) = &result {
        let mf = crate::master_fetch::BlpapiMasterFetcher { cfg, pool };
        if let Err(e) = crate::resolution::engine::auto_reresolve_invalid(
            pool, &mf, *run_id, chrono::Local::now().date_naive()).await {
            eprintln!("auto re-resolve after run {run_id} failed: {e}");
        }
    }
    result
```
(mirror in `run_backfill`).

- [x] **Step 4: Run to verify everything passes**

Run: `cd src-tauri && cargo test --test resolution -- --ignored && cargo test`
Expected: PASS (including all pre-existing resolution tests — none used the live wrappers).

- [x] **Step 5: Commit**

```bash
git add src-tauri/src/resolution/engine.rs src-tauri/src/orchestrator.rs src-tauri/tests/resolution.rs
git commit -m "feat: auto re-resolve dead securities by FIGI after live runs, with a 7-day cooldown"
```

---

### Task 8: Migrate and boot `bloomdata`

The app has never run against its real database (`relation "instrument" does not exist`). `db::connect` applies migrations at startup (`lib.rs:38`), so booting the app once is the migration.

**Files:** none (operational).

- [x] **Step 1: Create the database if missing**

```bash
PGPASSWORD=postgres psql -U postgres -h localhost -tc \
  "SELECT 1 FROM pg_database WHERE datname='bloomdata'" | grep -q 1 \
  || PGPASSWORD=postgres psql -U postgres -h localhost -c "CREATE DATABASE bloomdata"
```

- [x] **Step 2: Boot the app once**

Run `npm run tauri dev` (or the built exe) from the repo root; wait for the window; the startup panics loudly if migrations fail.

- [x] **Step 3: Verify the schema landed**

```bash
PGPASSWORD=postgres psql -U postgres -h localhost -d bloomdata -c "\dt" \
  | grep -E "instrument|observation|hit_ledger|non_trading_day"
```
Expected: all four present. Leave the app available for the user's GUI smoke pass (`docs/superpowers/plans/2026-08-19-p1-smoke-checklist.md` §"needs the GUI", priority list at the end of that file). Do not tick any smoke boxes — those are the user's to run against the live Terminal.

---

## Execution notes

- Tasks 3 → 6 and 3 → 7 are ordered by the migration dependency; Tasks 1, 2 are free-standing.
- After Task 8, the remaining P1 exit criteria are HUMAN steps: the 10 GUI smoke items, then merge to `master`.
