# P9: Multi-Asset Capability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop treating every instrument like a cash equity — per-asset-class capability flags (corp-actions on/off, M&A lifecycle on/off, adjustment style, class-default staleness) and a `'roll'` link type that splices futures series by difference instead of ratio.

**Architecture:** Four capability columns on `asset_class` (defaults keep existing behaviour), enforced at the four seams that currently assume equities: the corp-action estimate+refresh pair, `adjusted_series`, the quality gate's config query, and `lifecycle::investigate`. Stitching's multiplicative accumulator becomes affine `(mul, add)` so ratio and difference junctions compose correctly.

**Tech Stack:** Rust (sqlx/Postgres, Tauri 2), Svelte 5, SQL migrations (sqlx migrate).

**Spec:** `docs/superpowers/specs/2026-08-21-p9-p10-multi-asset-and-production-ops-design.md` (P9 half). Read it first.

## Global Constraints

- Never destroy data: observations/aliases/attrs/corp_actions/links are close-and-insert or append-only; DB triggers enforce it.
- Migration files MUST be LF-only (`.gitattributes` pins `src-tauri/migrations`); verify with `git ls-files --eol src-tauri/migrations` before committing. After adding a migration, `touch src-tauri/tests/common/mod.rs` so the embedded migration set refreshes.
- The corp-action gate estimate and the seam charge must count the same instruments — never change one query without the other (Task 3 changes both in one commit).
- There is deliberately **no hard budget cap** (standing user decision 2026-08-20).
- DB integration tests: `#[ignore = "requires postgres"]`, shared `bloom_test` DB via `tests/common/mod.rs::pool()`, every UNIQUE-constrained fixture value goes through `common::uniq()`. Tests never clean up.
- Test commands (from `src-tauri/`): `cargo test` (pure), `cargo test --no-fail-fast -- --ignored` (needs Postgres; `BLOOM_TEST_DATABASE_URL` or default `postgres://postgres:postgres@localhost/bloom_test`). Known permanent failure on bloom_test: `smoke_real_bloomberg_end_to_end` (state pollution; passes only on a fresh DB with a live Terminal). Frontend: `npm run check` from repo root (0 errors required; 1 pre-existing warning in ImportDiff.svelte is known).
- Every commit message ends with: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- House style: `//!` module docs explain WHY; comments only for non-obvious constraints; `AppResult`/`AppError`; advisory sub-steps log-and-continue (`eprintln!`) rather than failing a run that already ingested.

## File Structure

- Create: `src-tauri/migrations/0011_asset_class_capabilities.sql`
- Create: `src-tauri/migrations/0012_roll_link.sql`
- Create: `src-tauri/tests/capability.rs` (capability flags: schema, registry, corp-action gating, adjustment style, staleness default, lifecycle retire)
- Create: `docs/asset-class-playbook.md`
- Modify: `src-tauri/src/registry.rs` (AssetClass struct + update fn), `orchestrator.rs` (corp_actions_estimate), `corp_actions.rs` (refresh_view member filter), `adjust.rs` (style short-circuit), `quality.rs` (COALESCE config query), `lifecycle.rs` (retire path), `stitch.rs` (affine composer + roll), `commands.rs` + `lib.rs` (new commands), `src-tauri/tests/stitch.rs` (roll tests)
- Modify: `src/lib/api.ts`, `src/lib/SettingsScreen.svelte` (asset-class editor), `src/lib/InstrumentDetail.svelte` (roll-link form), `src/lib/DataScreen.svelte` (segment offset display)

---

### Task 1: Migration 0011 — capability flags on `asset_class`

**Files:**
- Create: `src-tauri/migrations/0011_asset_class_capabilities.sql`
- Test: `src-tauri/tests/capability.rs` (new file)

**Interfaces:**
- Produces: columns `asset_class.corp_actions_capable BOOLEAN NOT NULL DEFAULT TRUE`, `ma_capable BOOLEAN NOT NULL DEFAULT TRUE`, `adjustment_style TEXT NOT NULL DEFAULT 'factors'` (CHECK `'factors'|'none'`), `qc_stale_days_default INTEGER` (CHECK NULL or ≥ 2). Every later task reads these.

- [ ] **Step 1: Write the failing schema tests**

Create `src-tauri/tests/capability.rs`:

```rust
//! P9 capability flags: per-asset-class behaviour switches. See
//! docs/superpowers/specs/2026-08-21-p9-p10-multi-asset-and-production-ops-design.md.
mod common;

use common::uniq;

#[tokio::test]
#[ignore = "requires postgres"]
async fn asset_class_capabilities_default_to_equity_shaped() {
    let pool = common::pool().await;
    let row: (bool, bool, String, Option<i32>) = sqlx::query_as(
        "INSERT INTO asset_class (name) VALUES ($1)
         RETURNING corp_actions_capable, ma_capable, adjustment_style, qc_stale_days_default")
        .bind(uniq("CapDflt")).fetch_one(&pool).await.unwrap();
    assert_eq!(row, (true, true, "factors".to_string(), None));
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn adjustment_style_rejects_unknown_values() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, adjustment_style) VALUES ($1, 'sideways')")
        .bind(uniq("CapBad")).execute(&pool).await;
    assert!(err.is_err(), "unknown adjustment styles must be rejected");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn stale_default_below_two_is_rejected() {
    let pool = common::pool().await;
    let err = sqlx::query(
        "INSERT INTO asset_class (name, qc_stale_days_default) VALUES ($1, 1)")
        .bind(uniq("CapOne")).execute(&pool).await;
    assert!(err.is_err(), "a 1-day staleness window is meaningless (matches field_def CHECK)");
}
```

- [ ] **Step 2: Run tests to verify they fail correctly**

Run (from `src-tauri/`): `cargo test --test capability -- --ignored`
Expected: all 3 FAIL with a Postgres error naming the missing column (`corp_actions_capable` does not exist).

- [ ] **Step 3: Write the migration**

Create `src-tauri/migrations/0011_asset_class_capabilities.sql`:

```sql
-- P9: per-asset-class capability flags. Defaults keep every existing class
-- equity-shaped (corp actions on, M&A lifecycle on, factor adjustment, no
-- class staleness default), so no data migration is needed.
ALTER TABLE asset_class
  ADD COLUMN corp_actions_capable BOOLEAN NOT NULL DEFAULT TRUE,
  ADD COLUMN ma_capable           BOOLEAN NOT NULL DEFAULT TRUE,
  ADD COLUMN adjustment_style     TEXT NOT NULL DEFAULT 'factors'
      CONSTRAINT asset_class_adjustment_style_check
      CHECK (adjustment_style IN ('factors', 'none')),
  ADD COLUMN qc_stale_days_default INTEGER
      CONSTRAINT asset_class_qc_stale_default_min
      CHECK (qc_stale_days_default IS NULL OR qc_stale_days_default >= 2);
```

Then: `touch src-tauri/tests/common/mod.rs` (refresh the embedded migration set) and verify line endings: `git add src-tauri/migrations/0011_asset_class_capabilities.sql && git ls-files --eol src-tauri/migrations` — the new file must show `i/lf`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test --test capability -- --ignored`
Expected: 3 PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/migrations/0011_asset_class_capabilities.sql src-tauri/tests/capability.rs src-tauri/tests/common/mod.rs
git commit -m "feat(db): asset_class capability flags -- migration 0011"
```

---

### Task 2: Registry read/update + Settings UI for capabilities

**Files:**
- Modify: `src-tauri/src/registry.rs` (struct at :6, functions at :26/:32)
- Modify: `src-tauri/src/commands.rs` (near `list_asset_classes` at :71), `src-tauri/src/lib.rs` (invoke_handler list at :80-110)
- Modify: `src/lib/api.ts`, `src/lib/SettingsScreen.svelte`
- Test: `src-tauri/tests/capability.rs`

**Interfaces:**
- Consumes: Task 1 columns.
- Produces: `registry::AssetClass` with 4 new pub fields (`corp_actions_capable: bool`, `ma_capable: bool`, `adjustment_style: String`, `qc_stale_days_default: Option<i32>`); `registry::update_asset_class_capabilities(pool, id: i64, corp_actions_capable: bool, ma_capable: bool, adjustment_style: &str, qc_stale_days_default: Option<i32>) -> AppResult<()>`; Tauri command `update_asset_class_capabilities`. Existing `SELECT *` + `query_as` keep working (sqlx FromRow maps by name).

- [ ] **Step 1: Write the failing test** (append to `src-tauri/tests/capability.rs`)

```rust
use getbloomdata_lib::registry;

#[tokio::test]
#[ignore = "requires postgres"]
async fn capabilities_can_be_updated_and_read_back() {
    let pool = common::pool().await;
    let ac = registry::create_asset_class(&pool, &uniq("CapBond"), "").await.unwrap();
    registry::update_asset_class_capabilities(&pool, ac.id, false, false, "none", Some(8))
        .await.unwrap();
    let all = registry::list_asset_classes(&pool).await.unwrap();
    let got = all.iter().find(|c| c.id == ac.id).unwrap();
    assert!(!got.corp_actions_capable);
    assert!(!got.ma_capable);
    assert_eq!(got.adjustment_style, "none");
    assert_eq!(got.qc_stale_days_default, Some(8));
}
```

- [ ] **Step 2: Run to verify it fails** — `cargo test --test capability -- --ignored`
Expected: compile error — `update_asset_class_capabilities` and the struct fields don't exist.

- [ ] **Step 3: Implement**

In `src-tauri/src/registry.rs`, extend the struct (derive list unchanged) and add:

```rust
pub struct AssetClass {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub corp_actions_capable: bool,
    pub ma_capable: bool,
    pub adjustment_style: String,
    pub qc_stale_days_default: Option<i32>,
}

/// The CHECK constraints (style whitelist, stale >= 2) surface as AppError --
/// the UI relays them verbatim rather than pre-validating.
pub async fn update_asset_class_capabilities(
    pool: &PgPool, id: i64, corp_actions_capable: bool, ma_capable: bool,
    adjustment_style: &str, qc_stale_days_default: Option<i32>) -> AppResult<()>
{
    sqlx::query(
        "UPDATE asset_class
         SET corp_actions_capable = $2, ma_capable = $3,
             adjustment_style = $4, qc_stale_days_default = $5
         WHERE id = $1")
        .bind(id).bind(corp_actions_capable).bind(ma_capable)
        .bind(adjustment_style).bind(qc_stale_days_default)
        .execute(pool).await?;
    Ok(())
}
```

In `commands.rs` (next to `list_asset_classes`, :71):

```rust
#[tauri::command]
pub async fn update_asset_class_capabilities(state: State<'_, AppState>, id: i64,
    corp_actions_capable: bool, ma_capable: bool, adjustment_style: String,
    qc_stale_days_default: Option<i32>) -> Result<(), AppError>
{
    registry::update_asset_class_capabilities(&state.pool, id, corp_actions_capable,
        ma_capable, &adjustment_style, qc_stale_days_default).await
}
```

Register it in `lib.rs`'s `invoke_handler` list (alphabetical-ish, near the other asset-class commands at :81).

- [ ] **Step 4: Run tests** — `cargo test --test capability -- --ignored` → PASS; `cargo test` (unit sweep) → PASS.

- [ ] **Step 5: Frontend** — in `src/lib/api.ts` extend the `AssetClass` type with the 4 fields and add `updateAssetClassCapabilities(id, corpActionsCapable, maCapable, adjustmentStyle, qcStaleDaysDefault)` invoking `update_asset_class_capabilities` (mirror the argument-name casing convention of the neighbouring api.ts functions — Tauri camelCases Rust snake_case args). In `SettingsScreen.svelte`, add an "Asset classes" section after "Schedules": a table of `api.listAssetClasses()` rows with per-row controls — checkbox `corp_actions_capable` ("Corp actions"), checkbox `ma_capable` ("M&A lifecycle"), select `adjustment_style` (`factors`/`none`), number input `qc_stale_days_default` (empty = off, min 2) — and a per-row Save button calling the new api function then reloading the list. Match the existing section's markup/classes.

- [ ] **Step 6: Verify frontend** — `npm run check` → 0 errors.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/registry.rs src-tauri/src/commands.rs src-tauri/src/lib.rs src-tauri/tests/capability.rs src/lib/api.ts src/lib/SettingsScreen.svelte
git commit -m "feat: asset-class capability editor -- registry update + Settings UI"
```

---

### Task 3: Corp-action refresh honours `corp_actions_capable` (estimate + seam together)

**Files:**
- Modify: `src-tauri/src/orchestrator.rs` (`corp_actions_estimate`, :374-382)
- Modify: `src-tauri/src/corp_actions.rs` (`refresh_view` member query, :322+)
- Test: `src-tauri/tests/capability.rs`

**Interfaces:**
- Consumes: Task 1 columns.
- Produces: incapable-class members are invisible to both the pre-run estimate and the refresh; `ViewRefreshSummary` counts are computed over capable members only (an incapable member is excluded from the batch entirely — it does NOT increment `skipped`, which keeps meaning "no security today").

- [ ] **Step 1: Read both current queries.** Open `orchestrator.rs:374-382` and the member-selection query inside `corp_actions::refresh_view` (corp_actions.rs:322+). Both select view members and exclude `corp_actions_na`; note their exact FROM/WHERE shape before editing.

- [ ] **Step 2: Write the failing tests** (append to `src-tauri/tests/capability.rs`)

```rust
use chrono::NaiveDate;

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// One instrument in a corp-actions-capable class, one in an incapable class,
/// same view. Returns (view_id, capable_iid, incapable_iid).
async fn two_class_view(pool: &sqlx::PgPool, stem: &str) -> (i64, i64, i64) {
    let cap: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ($1) RETURNING id")
        .bind(uniq(&format!("{stem}Eq"))).fetch_one(pool).await.unwrap();
    let nocap: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name, corp_actions_capable) VALUES ($1, FALSE) RETURNING id")
        .bind(uniq(&format!("{stem}Bond"))).fetch_one(pool).await.unwrap();
    let mut ids = Vec::new();
    for class in [cap, nocap] {
        let inst = getbloomdata_lib::instrument::store::create(pool).await.unwrap();
        sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label) VALUES ($1,$2,$3)")
            .bind(inst.instrument_id).bind(class).bind(uniq(stem))
            .execute(pool).await.unwrap();
        ids.push(inst.instrument_id);
    }
    let view: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ($1) RETURNING id")
        .bind(uniq(&format!("{stem}V"))).fetch_one(pool).await.unwrap();
    for iid in &ids {
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
            .bind(view).bind(iid).execute(pool).await.unwrap();
    }
    (view, ids[0], ids[1])
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn corp_action_estimate_skips_incapable_classes() {
    let pool = common::pool().await;
    let (view, _cap, _nocap) = two_class_view(&pool, "CaEst").await;
    let est = getbloomdata_lib::orchestrator::corp_actions_estimate(&pool, view).await.unwrap();
    assert_eq!(est, 2, "1 capable member x 2 corp-action fields; the bond must not be counted");
}
```

Note: if `corp_actions_estimate` is not `pub`, making it `pub` is part of this task. If `two_class_view`'s inserts trip a NOT NULL you didn't expect, fix the fixture against `migrations/0001_init.sql` (book_entry at :248, view at :302) — do not weaken the assertion. Add a second test that `refresh_view` never requests the incapable member: mirror the mock-`MasterFetcher` pattern already used in `src-tauri/tests/corp_actions.rs` (a fetcher whose `corp_actions` records the securities it was asked for and returns empty tables — read that file first and reuse its scaffolding), give only the capable instrument a current `bdp_security` alias, and assert the recorded request list contains the capable security and nothing else, and that `summary.instruments == 1`.

- [ ] **Step 3: Run to verify failure** — `cargo test --test capability -- --ignored`. Expected: estimate test FAILS with `est == 4` (both members counted); refresh test FAILS with the bond security present in the recorded requests.

- [ ] **Step 4: Implement.** Add to both queries the same filter:

```sql
JOIN book_entry be ON be.instrument_id = vi.instrument_id
JOIN asset_class ac ON ac.id = be.asset_class_id
...
AND ac.corp_actions_capable
```

adapting alias names to each query's existing shape. Both changes in this one task — the gate's number and the seam's charge must count the same instruments (Global Constraints).

- [ ] **Step 5: Run tests** — `cargo test --test capability -- --ignored` → PASS. Also run the existing suite that pins the old behaviour: `cargo test --no-fail-fast --test corp_actions --test pipeline -- --ignored` → PASS (defaults are TRUE, so nothing else moves).

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/orchestrator.rs src-tauri/src/corp_actions.rs src-tauri/tests/capability.rs
git commit -m "feat: corp-action refresh + estimate skip incapable classes -- stop burning 2 hits/run on bonds"
```

---

### Task 4: `adjustment_style = 'none'` short-circuits the factor engine

**Files:**
- Modify: `src-tauri/src/adjust.rs` (`adjusted_series`, :104)
- Test: `src-tauri/tests/capability.rs`

**Interfaces:**
- Consumes: Task 1 columns; existing `adjust::adjusted_series(pool, instrument_id, field_id, mode, limit) -> AppResult<AdjSeries>`.
- Produces: for instruments whose class has `adjustment_style='none'`, `adjusted_series` returns `adjusted == raw`, `factors_used == 0`, `unusable_factors == 0` for every mode. `stitch::stitched_series` inherits automatically (it reads through `adjusted_series` at stitch.rs:194/:224).

- [ ] **Step 1: Write the failing test** (append to `src-tauri/tests/capability.rs`). Reuse the fixture idiom of `src-tauri/tests/adjust.rs` (scaffold at :14 — read it first; it inserts asset_class, instrument, field_def, a run, and raw observations with the RAW basis):

```rust
use getbloomdata_lib::adjust::{self, AdjustMode};

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_none_style_class_never_adjusts() {
    let pool = common::pool().await;
    // Scaffold exactly like tests/adjust.rs::scaffold but with
    // adjustment_style = 'none' on the class: instrument + field + one raw
    // observation of 100.0 on 2026-08-14, plus one flag-3 factor
    // (event_date 2026-08-17, amount 0.5, operator 1) that WOULD halve raw
    // history if the engine ran.
    // ... (copy the scaffold, add the column to the asset_class INSERT,
    //      copy the corp_action INSERT from adjust.rs's factor tests)
    let s = adjust::adjusted_series(&pool, iid, fid, AdjustMode::All, 100).await.unwrap();
    assert_eq!(s.rows.len(), 1);
    assert_eq!(s.rows[0].raw, s.rows[0].adjusted, "'none' style must bypass the factor chain");
    assert_eq!(s.factors_used, 0);
}
```

(The `// ...` is fixture assembly copied from `tests/adjust.rs` — the implementer copies the working scaffold, changing only the asset_class INSERT to add `adjustment_style` and the values noted. The assertions above are the contract and are not negotiable.)

- [ ] **Step 2: Run to verify failure** — `cargo test --test capability -- --ignored`. Expected: FAIL — `adjusted != raw` (the factor was applied; today `adjusted_series` never looks at the class).

- [ ] **Step 3: Implement.** In `adjust::adjusted_series`, after loading the raw rows and before the corp_action query (adjust.rs:124), insert:

```rust
// A class can opt out of adjustment entirely (yields, indices, futures):
// the factor chain is dividend/split arithmetic and is meaningless there.
let style: Option<String> = sqlx::query_scalar(
    "SELECT ac.adjustment_style FROM book_entry be
     JOIN asset_class ac ON ac.id = be.asset_class_id
     WHERE be.instrument_id = $1")
    .bind(instrument_id).fetch_optional(pool).await?;
if style.as_deref() == Some("none") {
    let rows = raw.into_iter()
        .map(|(obs_date, v)| AdjRow { obs_date, raw: v, adjusted: v })
        .collect();
    return Ok(AdjSeries { rows, factors_used: 0, unusable_factors: 0 });
}
```

adapting variable names to the function's actual locals (read adjust.rs:104-140 first). An instrument with no book_entry (`style == None`) keeps today's behaviour — fall through to the factor path.

- [ ] **Step 4: Run tests** — `cargo test --test capability -- --ignored` → PASS; `cargo test --no-fail-fast --test adjust --test stitch -- --ignored` → PASS (existing classes default to `'factors'`).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/adjust.rs src-tauri/tests/capability.rs
git commit -m "feat: adjustment_style 'none' bypasses the factor engine -- a yield never meets a split factor"
```

---

### Task 5: Class-default staleness window in the quality gate

**Files:**
- Modify: `src-tauri/src/quality.rs` (config batch query, :164-167)
- Test: `src-tauri/tests/capability.rs`

**Interfaces:**
- Consumes: Task 1 `qc_stale_days_default`; existing `QcConfig { nonpositive, outlier_pct, stale_days }` (quality.rs:14).
- Produces: effective stale window = `COALESCE(field_def.qc_stale_days, asset_class.qc_stale_days_default)`. Field-level explicit value always wins. `QcConfig` itself is unchanged — only the SQL that fills it.

- [ ] **Step 1: Write the failing test** (append to `src-tauri/tests/capability.rs`). Mirror the existing stale-detection test in `src-tauri/tests/quality.rs` (read it first — it ingests N identical values through a mock fetcher or direct observation inserts, then calls `quality::run_quality_gate` and asserts a `quality_stale` issue). The new test differs in exactly one way: `field_def.qc_stale_days` stays NULL and the class carries `qc_stale_days_default = 3`; three identical values must still produce `quality_stale`. Add a second precedence test: field-level `qc_stale_days = 2` with class default `= 5` — two identical values already fire (the field wins).

- [ ] **Step 2: Run to verify failure** — `cargo test --test capability -- --ignored`. Expected: first test FAILS with zero `quality_stale` issues (class default is invisible today).

- [ ] **Step 3: Implement.** Change the batch config query at quality.rs:164-167 from selecting `f.qc_stale_days` to:

```sql
SELECT f.id, f.qc_nonpositive, f.qc_outlier_pct,
       COALESCE(f.qc_stale_days, ac.qc_stale_days_default) AS qc_stale_days
FROM field_def f
JOIN asset_class ac ON ac.id = f.asset_class_id
WHERE f.id = ANY($1)
```

(adapt the SELECT list to the current query's exact columns — only the stale column changes shape). No Rust logic changes: `cfg_of` and the 200-row window arithmetic (quality.rs:186) already work off `QcConfig.stale_days`.

- [ ] **Step 4: Run tests** — `cargo test --test capability --test quality -- --ignored` → all PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/quality.rs src-tauri/tests/capability.rs
git commit -m "feat: class-default staleness window -- weekly-NAV funds stop crying wolf"
```

---

### Task 6: Lifecycle retire path for `ma_capable = FALSE`

**Files:**
- Modify: `src-tauri/src/lifecycle.rs` (`investigate`, :148; new `retire_path` next to `fund_path`, :274)
- Test: `src-tauri/tests/capability.rs`

**Interfaces:**
- Consumes: Task 1 `ma_capable`; existing `fund_path` (identity refresh + issue) and `record_issue(pool, instrument_id, code, detail, summary)` (lifecycle.rs:307, idempotent).
- Produces: a dead instrument in an `ma_capable = FALSE` class skips `ma_deals` entirely (zero M&A hits), gets the same identity refresh `fund_path` performs (so Bloomberg's `INACTIVE_DATE` caps the series via `close_attrs_at`), and records issue code **`lifecycle_retired`** with detail `"instrument inactive; class opted out of M&A investigation -- series capped at INACTIVE_DATE, retire the book entry"`. No link is proposed. This is the called-bond story (spec 9.3).

- [ ] **Step 1: Read `lifecycle.rs:148-305`** — `investigate`, `fund_path`, and the mock patterns in `src-tauri/tests/lifecycle.rs` (scaffold at :25, and the `MasterFetcher` mocks its tests use).

- [ ] **Step 2: Write the failing test** (append to `src-tauri/tests/capability.rs`). Reuse the `tests/lifecycle.rs` scaffold pattern but set `ma_capable = FALSE` on the class. The mock `MasterFetcher` must: answer `MARKET_STATUS` with an inactive status, serve the identity block, and **panic if `ma_deals` is called** (that panic IS the assertion that no M&A hit is burned). After `lifecycle::run_check`, assert:

```rust
let n: i64 = sqlx::query_scalar(
    "SELECT count(*) FROM ingest_issue
     WHERE instrument_id = $1 AND code = 'lifecycle_retired'")
    .bind(iid).fetch_one(&pool).await.unwrap();
assert_eq!(n, 1);
let links: i64 = sqlx::query_scalar(
    "SELECT count(*) FROM instrument_link WHERE predecessor_id = $1")
    .bind(iid).fetch_one(&pool).await.unwrap();
assert_eq!(links, 0, "a retired non-equity proposes no successor link");
```

- [ ] **Step 3: Run to verify failure** — expected: the mock's `ma_deals` panic fires (today `investigate` always calls it).

- [ ] **Step 4: Implement.** At the top of `investigate` (before the `ma_deals` call at lifecycle.rs:155):

```rust
let ma_capable: bool = sqlx::query_scalar(
    "SELECT ac.ma_capable FROM book_entry be
     JOIN asset_class ac ON ac.id = be.asset_class_id
     WHERE be.instrument_id = $1")
    .bind(instrument_id).fetch_optional(pool).await?
    .unwrap_or(true); // no book entry -> keep today's behaviour
if !ma_capable {
    return retire_path(pool, fetcher, instrument_id, summary).await;
}
```

`retire_path` mirrors `fund_path`'s identity-refresh body (same 6-year identifier-history ingest so `INACTIVE_DATE` lands and `close_attrs_at` fires) but records `lifecycle_retired` with the detail string from **Interfaces**, and increments `summary.dead`. Extract the shared identity-refresh block into a private helper if the duplication exceeds a few lines.

- [ ] **Step 5: Run tests** — `cargo test --test capability --test lifecycle -- --ignored` → PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/lifecycle.rs src-tauri/tests/capability.rs
git commit -m "feat: cap-and-retire lifecycle path for non-M&A classes -- a called bond ends like a delisted equity"
```

---

### Task 7: Migration 0012 — `'roll'` link type + `roll_offset`

**Files:**
- Create: `src-tauri/migrations/0012_roll_link.sql`
- Test: `src-tauri/tests/stitch.rs` (schema assertions appended)

**Interfaces:**
- Produces: `instrument_link.link_type` accepts `'roll'`; `instrument_link.roll_offset DOUBLE PRECISION` (signed, zero allowed, only on rolls). Semantics: **successor = predecessor + roll_offset at the junction**.

- [ ] **Step 1: Write the failing schema tests** (append to `src-tauri/tests/stitch.rs`, reusing its `scaffold` at :15 for two instruments):

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_roll_link_with_offset_is_accepted() {
    let pool = common::pool().await;
    let (a, _f, _) = scaffold(&pool, "RollA").await;
    let (b, _f2, _) = scaffold(&pool, "RollB").await;
    sqlx::query(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence, roll_offset)
         VALUES ($1, $2, 'roll', '2026-03-11', '{\"source\":\"test\"}', 2.5)")
        .bind(a).bind(b).execute(&pool).await
        .expect("'roll' must pass the link_type CHECK");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn roll_offset_is_rejected_on_a_merger() {
    let pool = common::pool().await;
    let (a, _f, _) = scaffold(&pool, "RollC").await;
    let (b, _f2, _) = scaffold(&pool, "RollD").await;
    let err = sqlx::query(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence, roll_offset)
         VALUES ($1, $2, 'merger', '2026-03-11', '{}', 2.5)")
        .bind(a).bind(b).execute(&pool).await;
    assert!(err.is_err(), "an additive offset is meaningless on a ratio link");
}
```

(Adapt the scaffold-return destructuring to its real signature — read `tests/stitch.rs:15` first.)

- [ ] **Step 2: Run to verify failure** — `cargo test --test stitch -- --ignored` (new tests only fail): first FAILS on the `link_type` CHECK, second FAILS because it succeeds-to-insert / unknown column.

- [ ] **Step 3: Write the migration** — `src-tauri/migrations/0012_roll_link.sql`:

```sql
-- P9: futures roll junctions splice by DIFFERENCE, not ratio. A roll link
-- carries a signed offset: successor = predecessor + roll_offset at the
-- junction. exchange_ratio CHECKs (> 0), so it cannot hold this.
ALTER TABLE instrument_link DROP CONSTRAINT instrument_link_link_type_check;
ALTER TABLE instrument_link ADD CONSTRAINT instrument_link_link_type_check
  CHECK (link_type IN ('rename', 'merger', 'conversion', 'share_class_change',
                       'spinoff', 'roll'));
ALTER TABLE instrument_link ADD COLUMN roll_offset DOUBLE PRECISION
  CONSTRAINT instrument_link_roll_offset_roll_only
  CHECK (roll_offset IS NULL OR link_type = 'roll');
```

(Precedent for the drop/re-add dance: `0009_run_kind_verify.sql`. If the DROP fails because Postgres auto-named the inline CHECK differently, find the real name with `SELECT conname FROM pg_constraint WHERE conrelid = 'instrument_link'::regclass AND contype = 'c';` and pin it.) Then `touch src-tauri/tests/common/mod.rs`; verify `i/lf` via `git ls-files --eol src-tauri/migrations`.

- [ ] **Step 4: Run tests** — `cargo test --test stitch -- --ignored` → new tests PASS, existing 5 still PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/migrations/0012_roll_link.sql src-tauri/tests/stitch.rs src-tauri/tests/common/mod.rs
git commit -m "feat(db): 'roll' link type with signed roll_offset -- migration 0012"
```

---

### Task 8: Affine stitch composer — difference splice

**Files:**
- Modify: `src-tauri/src/stitch.rs` (`LinkRow` :14, `Junction` :25, loader :166-176, mapping :71-76, composer :221-276, `SegmentInfo` :95)
- Modify: `src/lib/api.ts` (SegmentInfo type), `src/lib/DataScreen.svelte` (segment display)
- Test: `src-tauri/tests/stitch.rs`

**Interfaces:**
- Consumes: Task 7 column; existing `stitched_series(pool, instrument_id, field_id, mode, limit) -> AppResult<StitchedSeries>`.
- Produces: `LinkRow`/`Junction` gain `roll_offset: Option<f64>`; `SegmentInfo` gains `offset: Option<f64>` (per-junction, `None` on ratio segments; `ratio` is `None` on roll segments). Composer contract: value = `raw * mul + add`; ratio junction ⇒ `mul *= ratio`; roll junction with offset s ⇒ `add += s * mul`; volume series cross every junction unscaled (`mul` 1-equivalent, offset 0).

- [ ] **Step 1: Write the failing tests** (append to `src-tauri/tests/stitch.rs`; reuse its scaffold + observation-insert helpers — read the existing merger tests first and copy their fixture idiom):

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_roll_link_splices_by_difference() {
    // Predecessor B: obs 2026-03-10 = 98.0. Successor C: obs 2026-03-11 = 100.5.
    // Confirmed roll link B -> C effective 2026-03-11, roll_offset 2.5.
    // The stitched series for C must show B's day as 98.0 + 2.5 = 100.5,
    // and the B segment must carry offset Some(2.5), ratio None.
    // ... fixture as per existing merger tests, link INSERT with
    //     link_type 'roll', roll_offset 2.5, confirmed_by 'test' ...
    let s = stitch::stitched_series(&pool, c, fid, AdjustMode::Raw, 100).await.unwrap();
    let b_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-10")).unwrap();
    assert!((b_row.value - 100.5).abs() < 1e-9);
    let b_seg = s.segments.iter().find(|g| g.instrument_id == b).unwrap();
    assert_eq!(b_seg.offset, Some(2.5));
    assert_eq!(b_seg.ratio, None);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_ratio_junction_scales_a_deeper_rolls_offset() {
    // Chain: A --merger(exchange_ratio 2.0, junction ratio 0.5)--> B --roll(+3.0)--> C.
    // WRONG ORDER? No: walk from C backward -> roll first, then merger.
    // A value 10.0 maps: into B units 10*0.5 = 5.0; into C units 5.0 + 3.0 = 8.0.
    // Affine check: after roll (mul=1, add=3); after merger (mul=0.5, add=3);
    // 10*0.5 + 3 = 8.0.
    let a_row = s.rows.iter().find(|r| r.obs_date == d("2026-03-05")).unwrap();
    assert!((a_row.value - 8.0).abs() < 1e-9);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn a_roll_without_asserted_offset_derives_it_from_the_junction() {
    // roll link with roll_offset NULL; pred last obs before junction 98.0,
    // succ first obs on/after junction 101.0 -> derived offset 3.0.
    assert!((b_row.value - 101.0).abs() < 1e-9);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn volumes_cross_a_roll_unscaled() {
    // Same fixture, field mnemonic containing VOLUME -> concatenated verbatim.
    assert!((b_row.value - 50_000.0).abs() < 1e-9);
}
```

(`// ...` blocks are fixture assembly copied from the existing merger tests in the same file; the assertions are the contract.)

- [ ] **Step 2: Run to verify failure** — `cargo test --test stitch -- --ignored`. Expected: compile error on `SegmentInfo.offset` first; after stubbing the field, the difference test fails because the composer multiplies (value ≈ 98.0 or a stop).

- [ ] **Step 3: Implement.**
  1. `LinkRow` + `pub roll_offset: Option<f64>`; add `roll_offset` to the loader SELECT (stitch.rs:166-176) and the `Junction` mapping (:71-76).
  2. `SegmentInfo` + `pub offset: Option<f64>` (serde derives as the struct already has; set `None` in the target segment and ratio junctions).
  3. Composer (stitch.rs:221-276): replace `let mut cumulative = 1.0` with `let mut mul = 1.0_f64; let mut add = 0.0_f64;`. Per junction, in the existing precedence order:
     - volume ⇒ no change to `mul`/`add` (concat unscaled, existing note kept);
     - `rename`/`share_class_change` ⇒ no change;
     - **`roll`** ⇒ `let s = j.roll_offset.or_else(|| derive succ_val - pred_val from the same two-sided lookup the ratio fallback uses)`; on `None` set `stopped = format!("no junction offset at {d}: need one observation on each side")` and break; else `add += s * mul;` and record `offset: Some(s), ratio: None` on the segment;
     - otherwise (merger/conversion) ⇒ existing ratio logic, then `mul *= ratio;`.
     Apply at :276: `value: r.adjusted * mul + add`. The derived-offset lookup is the existing fallback at :256-269 with `-` instead of `/` and **no** zero guard.
  4. Frontend: `api.ts` SegmentInfo type gains `offset: number | null`; in `DataScreen.svelte` where a segment's ratio is rendered, also render the offset when present (e.g. `+2.5` / `-0.75`, sign always shown).

- [ ] **Step 4: Run tests** — `cargo test --test stitch --test currency -- --ignored` → all PASS (currency suite pins the P7 junction guard, which must be untouched — it already runs before the ratio/offset branch for every non-volume junction). `npm run check` → 0 errors.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/stitch.rs src-tauri/tests/stitch.rs src/lib/api.ts src/lib/DataScreen.svelte
git commit -m "feat: affine stitch composer -- roll junctions splice by difference, ratios still scale deeper offsets"
```

---

### Task 9: Manual roll-link creation (command + minimal UI)

**Files:**
- Modify: `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs` (register), `src-tauri/src/instrument/store.rs` (only if `propose_link`/`confirm_link` signatures need a thin wrapper)
- Modify: `src/lib/api.ts`, `src/lib/InstrumentDetail.svelte`
- Test: `src-tauri/tests/stitch.rs`

**Interfaces:**
- Consumes: Task 7/8; existing `store::propose_link` and `store::confirm_link` (read their signatures in `instrument/store.rs` — lifecycle.rs:242/:256 shows call shapes).
- Produces: Tauri command `create_roll_link(predecessor_id: i64, successor_id: i64, effective_date: String, roll_offset: Option<f64>) -> Result<i64, AppError>` returning the link id. The link is created `link_type='roll'`, evidence `{"source":"user"}`, `roll_offset` set, and **confirmed immediately** with `confirmed_by='user'` — the human typing it is the confirmation gate.

- [ ] **Step 1: Write the failing test** (append to `src-tauri/tests/stitch.rs`): call a new library fn `stitch::create_roll_link(pool, pred, succ, d("2026-03-11"), Some(2.5))` (put the logic in a library module so it is testable without Tauri `State`; the command is a thin wrapper), then assert the link row exists confirmed with the offset, and that `stitched_series` follows it (reuse Task 8's difference fixture minus the manual INSERT).

- [ ] **Step 2: Run to verify failure** — compile error: `create_roll_link` does not exist.

- [ ] **Step 3: Implement** `stitch::create_roll_link(pool, predecessor_id, successor_id, effective_date, roll_offset) -> AppResult<i64>`: validate `predecessor_id != successor_id` (the DB CHECK backs this), call `store::propose_link(…, "roll", effective_date, serde_json::json!({"source":"user"}))`, `UPDATE instrument_link SET roll_offset = $2 WHERE id = $1`, then `store::confirm_link(pool, link_id, "user")`. Command wrapper in `commands.rs` parses `effective_date` with `NaiveDate::parse_from_str(&effective_date, "%Y-%m-%d")`; register in `lib.rs`.

- [ ] **Step 4: Run tests** — `cargo test --test stitch -- --ignored` → PASS.

- [ ] **Step 5: Frontend** — `api.ts`: `createRollLink(predecessorId, successorId, effectiveDate, rollOffset)`. `InstrumentDetail.svelte`: in the links/predecessors area, an "Add roll link" disclosure with three inputs (predecessor instrument id — number; effective date — date input; offset — optional number, help text "successor = predecessor + offset; leave empty to derive from prices") and a submit that calls the api fn and reloads the detail. Match surrounding form markup. `npm run check` → 0 errors.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/stitch.rs src-tauri/src/commands.rs src-tauri/src/lib.rs src-tauri/tests/stitch.rs src/lib/api.ts src/lib/InstrumentDetail.svelte
git commit -m "feat: manual roll-link creation -- the human typing it is the confirmation gate"
```

---

### Task 10: Asset-class playbook

**Files:**
- Create: `docs/asset-class-playbook.md`

**Interfaces:** none (documentation). No test; review by reading.

- [ ] **Step 1: Write the playbook.** One table + one short section per class. Recommended settings (rationale in prose next to each):

| class | corp_actions | ma_capable | adjustment_style | qc_stale_days_default | typical fields |
|---|---|---|---|---|---|
| Equity | TRUE | TRUE | factors | NULL | PX_LAST, PX_VOLUME |
| Fund (weekly NAV) | FALSE | TRUE (fund path) | factors | 8 | FUND_NET_ASSET_VAL |
| Index | FALSE | FALSE | none | NULL | PX_LAST |
| FX | FALSE | FALSE | none | NULL | PX_LAST |
| Future | FALSE | FALSE | none | NULL | PX_LAST, PX_VOLUME; rolls via manual roll links |
| Fixed income | FALSE | FALSE | none | NULL | PX_LAST (clean), PX_DIRTY_MID, INT_ACC, YLD_YTM_MID |

Must-state caveats: yield fields keep `qc_nonpositive = FALSE` (negative yields are real); `GBp` prices are stored verbatim (P7 decision — pence stay pence); a called bond needs no link — `ma_capable = FALSE` + Bloomberg's `INACTIVE_DATE` cap the series (spec 9.3); funds keep `ma_capable = TRUE` because the fund path (absorption detection) rides the same investigation entry point.

- [ ] **Step 2: Commit**

```bash
git add docs/asset-class-playbook.md
git commit -m "docs: asset-class playbook -- recommended capability flags, fields, QC per class"
```

---

## Self-review checklist (run after writing, before handoff)

- Spec 9.1 → Tasks 1-6; 9.2 → Tasks 7-9; 9.3 → Tasks 6 + 10. Covered.
- Type consistency: `update_asset_class_capabilities` (Tasks 2), `roll_offset: Option<f64>` (Tasks 7-9), `SegmentInfo.offset: Option<f64>` (Task 8), `create_roll_link` (Task 9) — names match across tasks.
- Fixture-assembly `// ...` blocks always point at a named existing scaffold in the same repo and never replace an assertion.
