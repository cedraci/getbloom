# Deletion and Bulk Asset Management Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give the UI a way to remove every entity it can create, and a way to grow or shrink the asset book in bulk through an Excel round trip.

**Architecture:** A new `deletion` module owns per-entity Retire/Purge semantics as explicit transactional statements against restrictive foreign keys, exposed through one `describe_deletion` query plus five `delete_*` commands. A new `bulk` module splits the Excel path into three pieces with hard boundaries: `sheet.rs` touches files but never the database, `diff.rs` is a pure function over in-memory rows that touches neither, and `mod.rs` is the only place that does both. Two-phase import (preview, then apply against a SHA-256 of the same bytes) means a reviewed plan can never be applied to a changed file.

**Tech Stack:** Rust (Tauri 2, sqlx 0.8 / PostgreSQL 17, thiserror), `rust_xlsxwriter` for writing and `calamine` for reading `.xlsx`, `sha2` for the file hash, SvelteKit 5 (runes) + TypeScript for the UI.

**Spec:** `docs/superpowers/specs/2026-08-18-deletion-and-bulk-assets-design.md`

## Global Constraints

- **No migration.** This feature adds no SQL migration file. `asset`, `field_def`, `view` and `schedule` already carry `active`, and `views.rs` already filters on it (lines 77, 88, 101). If you believe you need a migration, stop and re-read spec §4 — you have probably misread the schema.
- **No `ON DELETE CASCADE` is added.** Purges are explicit `DELETE` statements in a documented order inside one transaction. Spec §3.3.
- **`run` and `hit_ledger` are never written or deleted by any code in this plan.** Spec §3.4. A purged asset leaves its runs and its budget ledger entries intact and truthful.
- **`bdp_security` is always recomputed** through `registry::resolve_bdp_security`. No code path in this plan may read a security string from a spreadsheet and store it. Spec §7.
- **`diff.rs` must not import `sqlx`, `std::fs`, `calamine`, or `rust_xlsxwriter`.** Its testability is the whole reason the `bulk/` directory exists as a deviation from this codebase's flat module style. A `use sqlx` in that file is a review rejection.
- **Any invalid row blocks the entire import.** Nothing is applied partially. Spec §9.
- **Every test in `src-tauri/tests/db_integration.rs` is `#[ignore]`.** Running `cargo test --test db_integration` reports "ok" while executing nothing. You **must** use `-- --ignored`. `BLOOM_TEST_DATABASE_URL` is set at User scope on this machine and is **not** inherited by an already-running shell — export it into the process first.
- **Integration-test fixtures never clean up and run in parallel against one database.** Every fixture name must go through the existing `uniq()` helper in `db_integration.rs`, or you will collide on `UNIQUE (bdp_security)` and `UNIQUE (asset_class.name)`.
- **The app holds a lock on `getbloomdata.exe`.** `cargo test` and `cargo build` fail with `os error 5 / Accès refusé` while it is running. Close the app first.

## Deviations from the spec, decided at planning time

Three details the spec left open, resolved here so no task has to invent them:

1. **`apply_assets_import` takes a fourth argument**, `confirmed_removal_count: Option<i64>`. Spec §8.1 guardrail 2 requires the user to type the removal count, but §6's signature has nowhere to put it. Task 10 adds it.
2. **No file-picker dialog.** `tauri-plugin-dialog` is not a dependency and adding it drags in capability configuration for one text field. Export and import take a `path: String`; the UI seeds that field from `get_settings().data_dir` as `<data_dir>\assets.xlsx`. The user can type any path.
3. **`active` flips are not "edits".** A row whose `active` differs from the database lands in `retires` or `reactivations`, never in an `EditRow.changed` list. This keeps the two ways of retiring an asset (the `active` column, and deleting the row entirely) visible as one category in the diff screen.

---

## Task 1: Deletion module and `describe_deletion`

**Files:**
- Create: `src-tauri/src/deletion.rs`
- Modify: `src-tauri/src/error.rs:4-15` (two new `AppError` variants)
- Modify: `src-tauri/src/lib.rs:1-12` (register the module)
- Test: `src-tauri/tests/db_integration.rs` (append)

**Interfaces:**
- Consumes: `crate::error::{AppError, AppResult}`; the existing `uniq()` helper in `db_integration.rs`.
- Produces:
  - `pub enum EntityKind { AssetClass, Asset, Field, View, Schedule }` — `Serialize`/`Deserialize`, `#[serde(rename_all = "snake_case")]`, `Clone + Copy + PartialEq + Eq + Debug`.
  - `pub enum DeleteMode { Retire, Purge }` — same derives and serde attribute.
  - `pub struct DeletionImpact` with fields `kind: EntityKind`, `id: i64`, `label: String`, `observations: i64`, `first_obs: Option<chrono::NaiveDate>`, `last_obs: Option<chrono::NaiveDate>`, `views: i64`, `issues: i64`, `runs: i64`, `children: i64`, `can_retire: bool`, `can_purge: bool`, `blocked_reason: Option<String>`.
  - `pub async fn describe_deletion(pool: &PgPool, kind: EntityKind, id: i64) -> AppResult<DeletionImpact>`
  - `AppError::DeleteBlocked { reason: String, counts: Vec<(String, i64)> }`
  - `AppError::ImportRejected { reason: String }`

- [ ] **Step 1: Add the two error variants**

In `src-tauri/src/error.rs`, add to the `AppError` enum, after `Validation`:

```rust
    #[error("cannot delete: {reason} ({})", format_counts(.counts))]
    DeleteBlocked { reason: String, counts: Vec<(String, i64)> },
    #[error("import rejected: {reason}")]
    ImportRejected { reason: String },
```

and below the enum, before `pub type AppResult`:

```rust
/// Render the blocking counts as "3 assets, 2 fields" for the Display impl.
/// The structured `counts` stay available to callers; this is only the text.
fn format_counts(counts: &[(String, i64)]) -> String {
    if counts.is_empty() {
        return "no details".into();
    }
    counts.iter()
        .map(|(what, n)| format!("{n} {what}"))
        .collect::<Vec<_>>()
        .join(", ")
}
```

- [ ] **Step 2: Register the module**

In `src-tauri/src/lib.rs`, add `pub mod deletion;` to the module list, keeping it alphabetical — between `pub mod db;` and `pub mod error;`.

- [ ] **Step 3: Write the failing test**

Append to `src-tauri/tests/db_integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn describe_deletion_counts_what_hangs_off_an_asset() {
    use getbloomdata_lib::deletion::{describe_deletion, EntityKind};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("DescribeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Describe Me".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("DSC"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("DescribeView"), "")
        .await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();

    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, source_run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, NULL),
                ($1, $2, DATE '2026-08-11', 2.0, NULL)")
        .bind(asset.id).bind(field.id)
        .execute(&pool).await.unwrap();

    let impact = describe_deletion(&pool, EntityKind::Asset, asset.id).await.unwrap();
    assert_eq!(impact.label, "Describe Me");
    assert_eq!(impact.observations, 2);
    assert_eq!(impact.first_obs, Some("2026-08-10".parse().unwrap()));
    assert_eq!(impact.last_obs, Some("2026-08-11".parse().unwrap()));
    assert_eq!(impact.views, 1);
    assert!(impact.can_retire && impact.can_purge);
    assert!(impact.blocked_reason.is_none());
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn describe_deletion_blocks_a_non_empty_asset_class() {
    use getbloomdata_lib::deletion::{describe_deletion, EntityKind};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("BlockedCls"), "test").await.unwrap();
    getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Occupant".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("OCC"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();

    let impact = describe_deletion(&pool, EntityKind::AssetClass, class.id).await.unwrap();
    assert_eq!(impact.children, 1);
    assert!(!impact.can_retire, "an asset class has no retired state");
    assert!(!impact.can_purge, "a class with an asset in it cannot be deleted");
    assert!(impact.blocked_reason.is_some());
}
```

- [ ] **Step 4: Run the tests to verify they fail**

```powershell
$env:BLOOM_TEST_DATABASE_URL = [Environment]::GetEnvironmentVariable("BLOOM_TEST_DATABASE_URL", "User")
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration describe_deletion -- --ignored --nocapture
```

Expected: compile error, `could not find 'deletion' in 'getbloomdata_lib'`.

- [ ] **Step 5: Write `src-tauri/src/deletion.rs`**

```rust
//! Removal of registry entities.
//!
//! Two shapes of removal, chosen per deletion rather than by a global setting:
//! Retire flips `active` (reversible, and already honoured by the fetch path in
//! `views.rs`), Purge deletes rows. Foreign keys stay restrictive on purpose --
//! see the design doc §3.3 -- so every purge spells out its DELETE order.
//!
//! `run` and `hit_ledger` are never touched. A run records work that really
//! happened and the ledger records budget that was really spent; both stay
//! truthful after the asset they mention is gone.

use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityKind {
    AssetClass,
    Asset,
    Field,
    View,
    Schedule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeleteMode {
    Retire,
    Purge,
}

/// What the confirm dialog shows. Every number comes from the database, so the
/// dialog never guesses at the blast radius.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DeletionImpact {
    pub kind: EntityKind,
    pub id: i64,
    pub label: String,
    pub observations: i64,
    pub first_obs: Option<chrono::NaiveDate>,
    pub last_obs: Option<chrono::NaiveDate>,
    /// Views this entity belongs to (assets and fields only).
    pub views: i64,
    pub issues: i64,
    /// Runs recorded against this view (views only).
    pub runs: i64,
    /// Rows that reference this one and stand in the way (classes: assets +
    /// fields; views: schedules).
    pub children: i64,
    pub can_retire: bool,
    pub can_purge: bool,
    pub blocked_reason: Option<String>,
}

async fn scalar(pool: &PgPool, sql: &str, id: i64) -> AppResult<i64> {
    let (n,): (i64,) = sqlx::query_as(sql).bind(id).fetch_one(pool).await?;
    Ok(n)
}

async fn label_of(pool: &PgPool, sql: &str, id: i64) -> AppResult<String> {
    let row: Option<(String,)> = sqlx::query_as(sql).bind(id).fetch_optional(pool).await?;
    row.map(|(l,)| l)
        .ok_or_else(|| AppError::Validation(format!("no such row: id {id}")))
}

pub async fn describe_deletion(
    pool: &PgPool,
    kind: EntityKind,
    id: i64,
) -> AppResult<DeletionImpact> {
    let mut impact = DeletionImpact {
        kind,
        id,
        label: String::new(),
        observations: 0,
        first_obs: None,
        last_obs: None,
        views: 0,
        issues: 0,
        runs: 0,
        children: 0,
        can_retire: false,
        can_purge: false,
        blocked_reason: None,
    };

    match kind {
        EntityKind::Asset => {
            impact.label = label_of(pool, "SELECT label FROM asset WHERE id = $1", id).await?;
            let dates: (i64, Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) =
                sqlx::query_as(
                    "SELECT COUNT(*), MIN(obs_date), MAX(obs_date)
                     FROM observation WHERE asset_id = $1")
                    .bind(id).fetch_one(pool).await?;
            impact.observations = dates.0;
            impact.first_obs = dates.1;
            impact.last_obs = dates.2;
            impact.views =
                scalar(pool, "SELECT COUNT(*) FROM view_asset WHERE asset_id = $1", id).await?;
            impact.issues =
                scalar(pool, "SELECT COUNT(*) FROM ingest_issue WHERE asset_id = $1", id).await?;
            impact.can_retire = true;
            impact.can_purge = true;
        }
        EntityKind::Field => {
            impact.label = label_of(pool, "SELECT label FROM field_def WHERE id = $1", id).await?;
            let dates: (i64, Option<chrono::NaiveDate>, Option<chrono::NaiveDate>) =
                sqlx::query_as(
                    "SELECT COUNT(*), MIN(obs_date), MAX(obs_date)
                     FROM observation WHERE field_id = $1")
                    .bind(id).fetch_one(pool).await?;
            impact.observations = dates.0;
            impact.first_obs = dates.1;
            impact.last_obs = dates.2;
            impact.views =
                scalar(pool, "SELECT COUNT(*) FROM view_field WHERE field_id = $1", id).await?;
            impact.issues =
                scalar(pool, "SELECT COUNT(*) FROM ingest_issue WHERE field_id = $1", id).await?;
            impact.can_retire = true;
            impact.can_purge = true;
        }
        EntityKind::View => {
            impact.label = label_of(pool, "SELECT name FROM view WHERE id = $1", id).await?;
            impact.runs = scalar(pool, "SELECT COUNT(*) FROM run WHERE view_id = $1", id).await?;
            impact.children =
                scalar(pool, "SELECT COUNT(*) FROM schedule WHERE view_id = $1", id).await?;
            impact.can_retire = true;
            impact.can_purge = impact.runs == 0;
            if !impact.can_purge {
                impact.blocked_reason = Some(format!(
                    "{} run(s) reference this view; retire it instead", impact.runs));
            }
        }
        EntityKind::AssetClass => {
            impact.label = label_of(pool, "SELECT name FROM asset_class WHERE id = $1", id).await?;
            let assets =
                scalar(pool, "SELECT COUNT(*) FROM asset WHERE asset_class_id = $1", id).await?;
            let flds =
                scalar(pool, "SELECT COUNT(*) FROM field_def WHERE asset_class_id = $1", id).await?;
            impact.children = assets + flds;
            impact.can_retire = false; // no `active` column, and none is needed
            impact.can_purge = impact.children == 0;
            if !impact.can_purge {
                impact.blocked_reason = Some(format!(
                    "{assets} asset(s) and {flds} field(s) still belong to this class"));
            }
        }
        EntityKind::Schedule => {
            let vid = scalar(pool, "SELECT view_id FROM schedule WHERE id = $1", id).await?;
            impact.label =
                label_of(pool, "SELECT name FROM view WHERE id = $1", vid).await?;
            impact.can_retire = false; // a schedule already has its own `active` toggle
            impact.can_purge = true;
        }
    }
    Ok(impact)
}
```

- [ ] **Step 6: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration describe_deletion -- --ignored --nocapture
```

Expected: `test result: ok. 2 passed`.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/deletion.rs src-tauri/src/error.rs src-tauri/src/lib.rs src-tauri/tests/db_integration.rs
git commit -m "feat: describe_deletion reports the blast radius of removing an entity"
```

---

## Task 2: Delete a schedule and delete an asset class

**Files:**
- Modify: `src-tauri/src/deletion.rs` (append two functions)
- Test: `src-tauri/tests/db_integration.rs` (append)

**Interfaces:**
- Consumes: `describe_deletion`, `DeletionImpact`, `EntityKind`, `AppError::DeleteBlocked` from Task 1.
- Produces:
  - `pub async fn delete_schedule(pool: &PgPool, id: i64) -> AppResult<()>`
  - `pub async fn delete_asset_class(pool: &PgPool, id: i64) -> AppResult<()>`

These two are the simple cases: neither has a Retire mode, so neither takes a `DeleteMode`.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/db_integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn delete_schedule_removes_the_row() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let view = getbloomdata_lib::views::create_view(&pool, &uniq("SchedView"), "").await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '18:00', TIME '19:00', true)")
        .bind(view.id).execute(&pool).await.unwrap();
    let (sid,): (i64,) = sqlx::query_as("SELECT id FROM schedule WHERE view_id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();

    getbloomdata_lib::deletion::delete_schedule(&pool, sid).await.unwrap();

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM schedule WHERE id = $1")
        .bind(sid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn delete_asset_class_refuses_while_occupied_then_succeeds_when_empty() {
    use getbloomdata_lib::error::AppError;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("EmptyMeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Tenant".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("TEN"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();

    let err = getbloomdata_lib::deletion::delete_asset_class(&pool, class.id).await.unwrap_err();
    assert!(matches!(err, AppError::DeleteBlocked { .. }),
            "expected DeleteBlocked, got {err:?}");

    sqlx::query("DELETE FROM asset WHERE id = $1").bind(asset.id)
        .execute(&pool).await.unwrap();
    getbloomdata_lib::deletion::delete_asset_class(&pool, class.id).await.unwrap();

    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset_class WHERE id = $1")
        .bind(class.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration delete_schedule delete_asset_class -- --ignored --nocapture
```

Expected: compile error, `cannot find function 'delete_schedule' in module 'deletion'`.

- [ ] **Step 3: Append the implementation to `src-tauri/src/deletion.rs`**

```rust
/// A schedule holds no history: there is nothing to retire and nothing to
/// cascade. `drawn_for`/`drawn_at` live on the row and go with it.
pub async fn delete_schedule(pool: &PgPool, id: i64) -> AppResult<()> {
    let n = sqlx::query("DELETE FROM schedule WHERE id = $1")
        .bind(id).execute(pool).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such schedule: id {id}")));
    }
    Ok(())
}

/// An asset class is a grouping, not data. It is deletable only when nothing
/// points at it -- there is no meaningful "retired class", because a retired
/// class would still have to answer for its assets.
pub async fn delete_asset_class(pool: &PgPool, id: i64) -> AppResult<()> {
    let assets = scalar(pool, "SELECT COUNT(*) FROM asset WHERE asset_class_id = $1", id).await?;
    let flds = scalar(pool, "SELECT COUNT(*) FROM field_def WHERE asset_class_id = $1", id).await?;
    if assets > 0 || flds > 0 {
        return Err(AppError::DeleteBlocked {
            reason: "asset class is not empty".into(),
            counts: vec![("asset(s)".into(), assets), ("field(s)".into(), flds)],
        });
    }
    let n = sqlx::query("DELETE FROM asset_class WHERE id = $1")
        .bind(id).execute(pool).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such asset class: id {id}")));
    }
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration delete_schedule delete_asset_class -- --ignored --nocapture
```

Expected: `test result: ok. 2 passed`.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/deletion.rs src-tauri/tests/db_integration.rs
git commit -m "feat: delete schedules, and asset classes once they are empty"
```

---

## Task 3: Retire and purge an asset, and the same for a field

**Files:**
- Modify: `src-tauri/src/deletion.rs` (append four functions)
- Test: `src-tauri/tests/db_integration.rs` (append)

**Interfaces:**
- Consumes: `DeleteMode` from Task 1.
- Produces:
  - `pub async fn purge_asset_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: i64) -> AppResult<()>`
  - `pub async fn purge_field_tx(tx: &mut sqlx::Transaction<'_, sqlx::Postgres>, id: i64) -> AppResult<()>`
  - `pub async fn delete_asset(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()>`
  - `pub async fn delete_field(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()>`

The `_tx` forms exist because Task 10's bulk apply must purge inside the *same* transaction as its adds and edits. The pool-taking forms are thin wrappers that open a transaction of their own.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/db_integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn retiring_an_asset_hides_it_from_views_but_keeps_its_observations() {
    use getbloomdata_lib::deletion::{delete_asset, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("RetireCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Retiree".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("RET"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("RetireView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, source_run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, NULL)")
        .bind(asset.id).bind(field.id).execute(&pool).await.unwrap();

    assert_eq!(getbloomdata_lib::views::view_assets(&pool, view.id).await.unwrap().len(), 1);

    delete_asset(&pool, asset.id, DeleteMode::Retire).await.unwrap();

    assert!(getbloomdata_lib::views::view_assets(&pool, view.id).await.unwrap().is_empty(),
            "a retired asset must drop out of view resolution");
    let (obs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM observation WHERE asset_id = $1")
        .bind(asset.id).fetch_one(&pool).await.unwrap();
    assert_eq!(obs, 1, "retire never destroys collected data");
    let (row,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset WHERE id = $1")
        .bind(asset.id).fetch_one(&pool).await.unwrap();
    assert_eq!(row, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_an_asset_clears_its_data_but_leaves_run_and_hit_ledger() {
    use getbloomdata_lib::deletion::{delete_asset, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("PurgeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Doomed".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("DOOM"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("PurgeView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();

    let (run_id,): (i64,) = sqlx::query_as(
        "INSERT INTO run (view_id, kind, trigger_kind, status, estimated_hits)
         VALUES ($1, 'eod', 'manual', 'completed', 1) RETURNING id")
        .bind(view.id).fetch_one(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, source_run_id)
         VALUES ($1, $2, DATE '2026-08-10', 1.0, $3)")
        .bind(asset.id).bind(field.id).bind(run_id).execute(&pool).await.unwrap();
    sqlx::query(
        "INSERT INTO ingest_issue (run_id, asset_id, field_id, obs_date, severity, code, detail)
         VALUES ($1, $2, $3, DATE '2026-08-10', 'warn', 'no_data', 'test')")
        .bind(run_id).bind(asset.id).bind(field.id).execute(&pool).await.unwrap();

    delete_asset(&pool, asset.id, DeleteMode::Purge).await.unwrap();

    for (table, col) in [("observation", "asset_id"), ("ingest_issue", "asset_id"),
                         ("view_asset", "asset_id"), ("asset", "id")] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE {col} = $1"))
            .bind(asset.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should have no rows for the purged asset");
    }
    let (runs,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM run WHERE id = $1")
        .bind(run_id).fetch_one(&pool).await.unwrap();
    assert_eq!(runs, 1, "a purge must never rewrite the record of work that happened");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_a_field_clears_its_observations_and_memberships() {
    use getbloomdata_lib::deletion::{delete_field, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("FldPurgeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Holder".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("HLD"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let field = getbloomdata_lib::fields::create_field(
        &pool, class.id, "PX_VOLUME", "Volume", "numeric").await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("FldPurgeView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_fields(&pool, view.id, &[field.id]).await.unwrap();
    sqlx::query(
        "INSERT INTO observation (asset_id, field_id, obs_date, value_num, source_run_id)
         VALUES ($1, $2, DATE '2026-08-10', 5.0, NULL)")
        .bind(asset.id).bind(field.id).execute(&pool).await.unwrap();

    delete_field(&pool, field.id, DeleteMode::Purge).await.unwrap();

    for (table, col) in [("observation", "field_id"), ("ingest_issue", "field_id"),
                         ("view_field", "field_id"), ("field_def", "id")] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE {col} = $1"))
            .bind(field.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should have no rows for the purged field");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration retiring_an_asset purging_an_asset purging_a_field -- --ignored --nocapture
```

Expected: compile error, `cannot find function 'delete_asset' in module 'deletion'`.

- [ ] **Step 3: Append the implementation to `src-tauri/src/deletion.rs`**

```rust
/// Purge order matters: children before parents, because the foreign keys are
/// deliberately restrictive. `ingest_issue` first (it names both an asset and a
/// run), then `observation`, then the membership row, then the asset itself.
pub async fn purge_asset_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> AppResult<()> {
    sqlx::query("DELETE FROM ingest_issue WHERE asset_id = $1")
        .bind(id).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM observation WHERE asset_id = $1")
        .bind(id).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM view_asset WHERE asset_id = $1")
        .bind(id).execute(&mut **tx).await?;
    let n = sqlx::query("DELETE FROM asset WHERE id = $1")
        .bind(id).execute(&mut **tx).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such asset: id {id}")));
    }
    Ok(())
}

pub async fn purge_field_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: i64,
) -> AppResult<()> {
    sqlx::query("DELETE FROM ingest_issue WHERE field_id = $1")
        .bind(id).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM observation WHERE field_id = $1")
        .bind(id).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM view_field WHERE field_id = $1")
        .bind(id).execute(&mut **tx).await?;
    let n = sqlx::query("DELETE FROM field_def WHERE id = $1")
        .bind(id).execute(&mut **tx).await?.rows_affected();
    if n == 0 {
        return Err(AppError::Validation(format!("no such field: id {id}")));
    }
    Ok(())
}

pub async fn delete_asset(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()> {
    match mode {
        DeleteMode::Retire => {
            let n = sqlx::query("UPDATE asset SET active = false WHERE id = $1")
                .bind(id).execute(pool).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such asset: id {id}")));
            }
            Ok(())
        }
        DeleteMode::Purge => {
            let mut tx = pool.begin().await?;
            purge_asset_tx(&mut tx, id).await?;
            tx.commit().await?;
            Ok(())
        }
    }
}

pub async fn delete_field(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()> {
    match mode {
        DeleteMode::Retire => {
            let n = sqlx::query("UPDATE field_def SET active = false WHERE id = $1")
                .bind(id).execute(pool).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such field: id {id}")));
            }
            Ok(())
        }
        DeleteMode::Purge => {
            let mut tx = pool.begin().await?;
            purge_field_tx(&mut tx, id).await?;
            tx.commit().await?;
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration retiring_an_asset purging_an_asset purging_a_field -- --ignored --nocapture
```

Expected: `test result: ok. 3 passed`.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/deletion.rs src-tauri/tests/db_integration.rs
git commit -m "feat: retire or purge assets and fields, leaving run history intact"
```

---

## Task 4: Delete a view, and stop the scheduler firing retired views

**Files:**
- Modify: `src-tauri/src/deletion.rs` (append one function)
- Modify: `src-tauri/src/scheduler.rs:92-94` (add the `view.active` filter)
- Test: `src-tauri/tests/db_integration.rs` (append)

**Interfaces:**
- Consumes: `DeleteMode`, `AppError::DeleteBlocked`.
- Produces: `pub async fn delete_view(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()>`

This is the task that closes spec §4's one open question. `scheduler::tick` currently selects `FROM schedule WHERE active` with no reference to the view, so retiring a view today leaves its schedule firing. That is the bug this task fixes.

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/db_integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn a_view_with_runs_refuses_to_purge_but_still_retires() {
    use getbloomdata_lib::deletion::{delete_view, DeleteMode};
    use getbloomdata_lib::error::AppError;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let view = getbloomdata_lib::views::create_view(&pool, &uniq("HistoricView"), "").await.unwrap();
    sqlx::query(
        "INSERT INTO run (view_id, kind, trigger_kind, status, estimated_hits)
         VALUES ($1, 'eod', 'manual', 'completed', 1)")
        .bind(view.id).execute(&pool).await.unwrap();

    let err = delete_view(&pool, view.id, DeleteMode::Purge).await.unwrap_err();
    assert!(matches!(err, AppError::DeleteBlocked { .. }), "got {err:?}");

    delete_view(&pool, view.id, DeleteMode::Retire).await.unwrap();
    let (active,): (bool,) = sqlx::query_as("SELECT active FROM view WHERE id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();
    assert!(!active);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn purging_a_never_run_view_takes_its_schedule_and_memberships_with_it() {
    use getbloomdata_lib::deletion::{delete_view, DeleteMode};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("VPurgeCls"), "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Member".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("MEM"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();
    let view = getbloomdata_lib::views::create_view(&pool, &uniq("FreshView"), "").await.unwrap();
    getbloomdata_lib::views::set_view_assets(&pool, view.id, &[asset.id]).await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '18:00', TIME '19:00', true)")
        .bind(view.id).execute(&pool).await.unwrap();

    delete_view(&pool, view.id, DeleteMode::Purge).await.unwrap();

    for table in ["schedule", "view_asset", "view_field"] {
        let (n,): (i64,) = sqlx::query_as(
            &format!("SELECT COUNT(*) FROM {table} WHERE view_id = $1"))
            .bind(view.id).fetch_one(&pool).await.unwrap();
        assert_eq!(n, 0, "{table} should be empty for the purged view");
    }
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM view WHERE id = $1")
        .bind(view.id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0);
    // The asset itself is untouched: a view is a selection, not an owner.
    let (a,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset WHERE id = $1")
        .bind(asset.id).fetch_one(&pool).await.unwrap();
    assert_eq!(a, 1);
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn scheduler_skips_schedules_whose_view_is_retired() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();

    let view = getbloomdata_lib::views::create_view(&pool, &uniq("RetiredSched"), "").await.unwrap();
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, TIME '00:01', TIME '00:02', true)")
        .bind(view.id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE view SET active = false WHERE id = $1")
        .bind(view.id).execute(&pool).await.unwrap();

    let due = getbloomdata_lib::scheduler::due_schedules(&pool).await.unwrap();
    assert!(!due.iter().any(|(_, vid, _)| *vid == view.id),
            "a retired view must not appear in the due list");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration a_view_with_runs purging_a_never_run_view scheduler_skips -- --ignored --nocapture
```

Expected: compile errors for `deletion::delete_view` and `scheduler::due_schedules`.

- [ ] **Step 3: Append `delete_view` to `src-tauri/src/deletion.rs`**

```rust
/// A view with runs behind it cannot honestly be purged: `run.view_id` would
/// dangle, and runs are never rewritten (design §3.4). Retire is always
/// available, and -- once the scheduler filters on `view.active` -- retiring
/// genuinely stops collection.
///
/// `view_asset` and `view_field` are the only cascading foreign keys in the
/// schema, so they go with the view without an explicit statement.
pub async fn delete_view(pool: &PgPool, id: i64, mode: DeleteMode) -> AppResult<()> {
    match mode {
        DeleteMode::Retire => {
            let n = sqlx::query("UPDATE view SET active = false WHERE id = $1")
                .bind(id).execute(pool).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such view: id {id}")));
            }
            Ok(())
        }
        DeleteMode::Purge => {
            let runs = scalar(pool, "SELECT COUNT(*) FROM run WHERE view_id = $1", id).await?;
            if runs > 0 {
                return Err(AppError::DeleteBlocked {
                    reason: "this view has been run; retire it instead of purging".into(),
                    counts: vec![("run(s)".into(), runs)],
                });
            }
            let mut tx = pool.begin().await?;
            sqlx::query("DELETE FROM schedule WHERE view_id = $1")
                .bind(id).execute(&mut *tx).await?;
            let n = sqlx::query("DELETE FROM view WHERE id = $1")
                .bind(id).execute(&mut *tx).await?.rows_affected();
            if n == 0 {
                return Err(AppError::Validation(format!("no such view: id {id}")));
            }
            tx.commit().await?;
            Ok(())
        }
    }
}
```

- [ ] **Step 4: Extract and fix the scheduler's due query**

In `src-tauri/src/scheduler.rs`, replace the inline query inside `tick` (currently lines 92-94):

```rust
    let schedules: Vec<(i64, i64, Option<String>)> = sqlx::query_as(
        "SELECT id, view_id, last_result FROM schedule WHERE active")
        .fetch_all(pool).await?;
```

with a call to a named function:

```rust
    let schedules = due_schedules(pool).await?;
```

and add this function to the module, immediately above `tick`:

```rust
/// Schedules eligible to fire. A schedule is due only when BOTH it and its view
/// are active -- retiring a view has to stop its scheduled runs, or "retire"
/// would mean nothing for the one entity that drives collection.
pub async fn due_schedules(pool: &PgPool) -> AppResult<Vec<(i64, i64, Option<String>)>> {
    Ok(sqlx::query_as(
        "SELECT s.id, s.view_id, s.last_result
         FROM schedule s JOIN view v ON v.id = s.view_id
         WHERE s.active AND v.active")
        .fetch_all(pool).await?)
}
```

Check the imports at the top of `scheduler.rs`: it already uses `PgPool` and `AppResult`. If either is missing from the `use` list, add it.

- [ ] **Step 5: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration a_view_with_runs purging_a_never_run_view scheduler_skips -- --ignored --nocapture
```

Expected: `test result: ok. 3 passed`.

- [ ] **Step 6: Run the whole existing suite to confirm the scheduler change broke nothing**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration -- --ignored
```

Expected: all green. There are 39 unit tests and (before this plan) 8 integration tests.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src/deletion.rs src-tauri/src/scheduler.rs src-tauri/tests/db_integration.rs
git commit -m "feat: delete views, and stop the scheduler firing retired ones"
```

---

## Task 5: Wire the deletion commands into Tauri and the TypeScript client

**Files:**
- Modify: `src-tauri/src/commands.rs` (append a deletion section)
- Modify: `src-tauri/src/lib.rs:68-77` (extend `generate_handler!`)
- Modify: `src/lib/api.ts` (types and six client methods)

**Interfaces:**
- Consumes: everything from `deletion` in Tasks 1-4.
- Produces, callable from the frontend:
  - `describe_deletion(kind, id) -> DeletionImpact`
  - `delete_asset(id, mode)`, `delete_field(id, mode)`, `delete_view(id, mode)`
  - `delete_asset_class(id)`, `delete_schedule(id)`
  - TypeScript: `EntityKind`, `DeleteMode`, `DeletionImpact`, and `api.describeDeletion`, `api.deleteAsset`, `api.deleteField`, `api.deleteView`, `api.deleteAssetClass`, `api.deleteSchedule`.

There is no test step here: this is pure wiring, verified by `cargo build` and `svelte-check`. Task 6 exercises it through the UI.

- [ ] **Step 1: Append the command section to `src-tauri/src/commands.rs`**

Add at the end of the file, and add `deletion` to the `use crate::{...}` list on line 4:

```rust
// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------
//
// The UI calls describe_deletion to render its dialog, but every delete_*
// command re-checks its own invariants. The dialog is a courtesy; the command
// is the enforcement.

#[tauri::command]
pub async fn describe_deletion(state: State<'_, AppState>,
                               kind: deletion::EntityKind, id: i64)
    -> Result<deletion::DeletionImpact, AppError> {
    deletion::describe_deletion(&state.pool, kind, id).await
}

#[tauri::command]
pub async fn delete_asset(state: State<'_, AppState>, id: i64, mode: deletion::DeleteMode)
    -> Result<(), AppError> {
    deletion::delete_asset(&state.pool, id, mode).await
}

#[tauri::command]
pub async fn delete_field(state: State<'_, AppState>, id: i64, mode: deletion::DeleteMode)
    -> Result<(), AppError> {
    deletion::delete_field(&state.pool, id, mode).await
}

#[tauri::command]
pub async fn delete_view(state: State<'_, AppState>, id: i64, mode: deletion::DeleteMode)
    -> Result<(), AppError> {
    deletion::delete_view(&state.pool, id, mode).await
}

#[tauri::command]
pub async fn delete_asset_class(state: State<'_, AppState>, id: i64)
    -> Result<(), AppError> {
    deletion::delete_asset_class(&state.pool, id).await
}

#[tauri::command]
pub async fn delete_schedule(state: State<'_, AppState>, id: i64)
    -> Result<(), AppError> {
    deletion::delete_schedule(&state.pool, id).await
}
```

- [ ] **Step 2: Register them in `src-tauri/src/lib.rs`**

Inside `tauri::generate_handler![...]`, after the `commands::list_schedules, commands::upsert_schedule,` line, add:

```rust
            commands::describe_deletion,
            commands::delete_asset, commands::delete_field, commands::delete_view,
            commands::delete_asset_class, commands::delete_schedule,
```

- [ ] **Step 3: Build to verify**

```powershell
cargo build --manifest-path src-tauri/Cargo.toml
```

Expected: compiles clean. If it fails with `os error 5 / Accès refusé`, close the running app and retry.

- [ ] **Step 4: Add the TypeScript types and client methods to `src/lib/api.ts`**

Add after the `ScheduleRow` interface:

```typescript
export type EntityKind = "asset_class" | "asset" | "field" | "view" | "schedule";
export type DeleteMode = "retire" | "purge";
export interface DeletionImpact {
  kind: EntityKind; id: number; label: string;
  observations: number; first_obs: string | null; last_obs: string | null;
  views: number; issues: number; runs: number; children: number;
  can_retire: boolean; can_purge: boolean; blocked_reason: string | null;
}
```

and inside the `api` object, after `upsertSchedule`:

```typescript
  describeDeletion: (kind: EntityKind, id: number) =>
    invoke<DeletionImpact>("describe_deletion", { kind, id }),
  deleteAsset: (id: number, mode: DeleteMode) => invoke<void>("delete_asset", { id, mode }),
  deleteField: (id: number, mode: DeleteMode) => invoke<void>("delete_field", { id, mode }),
  deleteView: (id: number, mode: DeleteMode) => invoke<void>("delete_view", { id, mode }),
  deleteAssetClass: (id: number) => invoke<void>("delete_asset_class", { id }),
  deleteSchedule: (id: number) => invoke<void>("delete_schedule", { id }),
```

- [ ] **Step 5: Type-check the frontend**

```powershell
npm run check
```

Expected: 0 errors.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src/lib/api.ts
git commit -m "feat: expose the deletion commands to the frontend"
```

---

## Task 6: `DeleteDialog.svelte` and the delete controls

**Files:**
- Create: `src/lib/DeleteDialog.svelte`
- Modify: `src/lib/AssetsScreen.svelte` (delete controls for classes and assets)
- Modify: `src/lib/ViewsScreen.svelte` (delete controls for fields, views and schedules)

**Interfaces:**
- Consumes: `api.describeDeletion`, `api.deleteAsset`, `api.deleteField`, `api.deleteView`, `api.deleteAssetClass`, `api.deleteSchedule`, `DeletionImpact`, `EntityKind`, `DeleteMode` from Task 5.
- Produces: a Svelte 5 component with props `{ kind: EntityKind, id: number, onclose: (changed: boolean) => void }`. It fetches its own `DeletionImpact` on mount and calls `onclose(true)` after a successful delete, `onclose(false)` on cancel.

This task completes the deletion half of the feature. After it, deletion ships standalone and Tasks 7-13 can be scheduled separately.

- [ ] **Step 1: Create `src/lib/DeleteDialog.svelte`**

```svelte
<script lang="ts">
  import { api, type DeleteMode, type DeletionImpact, type EntityKind } from "./api";

  let { kind, id, onclose }: {
    kind: EntityKind; id: number; onclose: (changed: boolean) => void;
  } = $props();

  let impact = $state<DeletionImpact | null>(null);
  let error = $state("");
  let busy = $state(false);

  $effect(() => {
    api.describeDeletion(kind, id)
      .then((i) => (impact = i))
      .catch((e) => (error = String(e)));
  });

  const NOUN: Record<EntityKind, string> = {
    asset_class: "asset class", asset: "asset", field: "field",
    view: "view", schedule: "schedule",
  };

  async function run(mode: DeleteMode) {
    busy = true; error = "";
    try {
      if (kind === "asset") await api.deleteAsset(id, mode);
      else if (kind === "field") await api.deleteField(id, mode);
      else if (kind === "view") await api.deleteView(id, mode);
      else if (kind === "asset_class") await api.deleteAssetClass(id);
      else await api.deleteSchedule(id);
      onclose(true);
    } catch (e) { error = String(e); busy = false; }
  }
</script>

<div class="backdrop">
  <div class="dialog">
    {#if error}<p class="error">{error}</p>{/if}
    {#if !impact}
      <p>Checking what depends on this&hellip;</p>
    {:else}
      <h3>Remove {NOUN[kind]} &ldquo;{impact.label}&rdquo;?</h3>
      <ul class="counts">
        {#if impact.observations > 0}
          <li>{impact.observations} observation(s), {impact.first_obs} to {impact.last_obs}</li>
        {/if}
        {#if impact.views > 0}<li>member of {impact.views} view(s)</li>{/if}
        {#if impact.issues > 0}<li>{impact.issues} recorded issue(s)</li>{/if}
        {#if impact.runs > 0}<li>{impact.runs} run(s) reference it</li>{/if}
        {#if impact.children > 0}<li>{impact.children} dependent row(s)</li>{/if}
        {#if impact.observations === 0 && impact.views === 0 && impact.issues === 0
             && impact.runs === 0 && impact.children === 0}
          <li>nothing depends on it</li>
        {/if}
      </ul>
      {#if impact.blocked_reason}<p class="blocked">{impact.blocked_reason}</p>{/if}
      <div class="actions">
        {#if impact.can_retire}
          <button onclick={() => run("retire")} disabled={busy}>
            Retire &mdash; stop collecting, keep the data
          </button>
        {/if}
        {#if impact.can_purge}
          <button class="danger" onclick={() => run("purge")} disabled={busy}>
            {impact.can_retire ? "Purge \u2014 delete it and its data" : "Delete"}
          </button>
        {/if}
        <button onclick={() => onclose(false)} disabled={busy}>Cancel</button>
      </div>
      {#if impact.can_purge && impact.can_retire}
        <p class="note">Purge cannot be undone. Runs and the budget ledger are never altered.</p>
      {/if}
    {/if}
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.35);
              display: flex; align-items: center; justify-content: center; }
  .dialog { background: #fff; border-radius: 4px; padding: 1.2rem;
            max-width: 34rem; box-shadow: 0 4px 20px rgba(0,0,0,0.3); }
  h3 { margin: 0 0 0.6rem; }
  .counts { margin: 0 0 0.8rem; padding-left: 1.2rem; color: #444; }
  .blocked { color: #a60; margin: 0 0 0.8rem; }
  .error { color: #c00; }
  .note { color: #666; font-size: 0.85rem; margin: 0.8rem 0 0; }
  .actions { display: flex; gap: 0.5rem; flex-wrap: wrap; }
  .danger { color: #c00; }
</style>
```

- [ ] **Step 2: Add the controls to `src/lib/AssetsScreen.svelte`**

In the `<script>` block, add the import and one piece of state:

```typescript
  import DeleteDialog from "./DeleteDialog.svelte";
  import type { EntityKind } from "./api";
  let pending = $state<{ kind: EntityKind; id: number } | null>(null);

  function afterDelete(changed: boolean) {
    pending = null;
    if (changed) reload();
  }
```

Replace the class list with one that carries a remove button:

```svelte
  <ul class="classes">
    {#each classes as c}
      <li>{c.name}
        <button class="x" title="Remove class"
                onclick={() => (pending = { kind: "asset_class", id: c.id })}>&times;</button>
      </li>
    {/each}
  </ul>
```

Add a header cell and a body cell to the assets table:

```svelte
    <thead><tr><th>Label</th><th>Security</th><th>Class</th><th>Active</th><th></th></tr></thead>
```

and inside the `{#each assets as a}` row, after the Active cell:

```svelte
          <td><button class="x" title="Remove asset"
                      onclick={() => (pending = { kind: "asset", id: a.id })}>&times;</button></td>
```

Finally, at the very end of the markup, before `<style>`:

```svelte
{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id} onclose={afterDelete} />
{/if}
```

and add to the `<style>` block:

```css
  .x { border: none; background: none; color: #c00; cursor: pointer;
       font-size: 1rem; line-height: 1; padding: 0 0.3rem; }
```

- [ ] **Step 3: Add the same controls to `src/lib/ViewsScreen.svelte`**

Add the identical `import DeleteDialog`, `pending` state, `afterDelete` handler, `.x` style rule, and trailing `{#if pending}` block as in Step 2.

Then add a remove button next to each view, each field, and each schedule row, using the matching kind:

```svelte
  <button class="x" title="Remove view"
          onclick={() => (pending = { kind: "view", id: v.id })}>&times;</button>
```

```svelte
  <button class="x" title="Remove field"
          onclick={() => (pending = { kind: "field", id: f.id })}>&times;</button>
```

```svelte
  <button class="x" title="Remove schedule"
          onclick={() => (pending = { kind: "schedule", id: s.id })}>&times;</button>
```

Read the file first and place each button inside the existing list or table row for that entity; do not restructure the markup around it.

- [ ] **Step 4: Type-check and build**

```powershell
npm run check
npm run build
```

Expected: 0 errors from `svelte-check`, and a successful Vite build.

- [ ] **Step 5: Manual smoke test**

```powershell
cargo build --manifest-path src-tauri/Cargo.toml
.\src-tauri\target\debug\getbloomdata.exe
```

In the app: create a throwaway asset class, try to delete it while an asset belongs to it (expect the blocked message), retire an asset (expect it to vanish from its view's asset list), and purge the leftover `AAPL US Equity Equity` row that migration 0004 deactivated. Close the app when done.

- [ ] **Step 6: Commit**

```bash
git add src/lib/DeleteDialog.svelte src/lib/AssetsScreen.svelte src/lib/ViewsScreen.svelte
git commit -m "feat: delete controls and impact dialog in the assets and views screens"
```

**Deletion is complete and shippable at this point.** Tasks 7-13 build the bulk path.

---

## Task 7: Sheet writer

**Files:**
- Create: `src-tauri/src/bulk/mod.rs` (module declarations only, for now)
- Create: `src-tauri/src/bulk/sheet.rs`
- Modify: `src-tauri/Cargo.toml` (three dependencies)
- Modify: `src-tauri/src/lib.rs` (register `pub mod bulk;`)
- Test: unit tests inside `src-tauri/src/bulk/sheet.rs`

**Interfaces:**
- Consumes: `crate::error::{AppError, AppResult}`.
- Produces:
  - `pub struct ExportRow { pub id: i64, pub label: String, pub class: String, pub id_kind: String, pub ticker: String, pub isin: String, pub yellow_key: String, pub active: bool, pub security: String, pub views: Vec<String> }`
  - `pub const SHEET_NAME: &str = "Assets";`
  - `pub const FIXED_HEADERS: [&str; 9] = ["id", "label", "class", "id_kind", "ticker", "isin", "yellow_key", "active", "security"];`
  - `pub fn write_assets_sheet(path: &Path, rows: &[ExportRow], view_names: &[String], class_names: &[String]) -> AppResult<()>`
  - `pub fn file_sha256(path: &Path) -> AppResult<String>`

`sheet.rs` touches the filesystem and never the database. That split is what lets Task 9's differ be a pure function.

- [ ] **Step 1: Add the dependencies**

```powershell
cargo add --manifest-path src-tauri/Cargo.toml rust_xlsxwriter calamine sha2
```

Then open `src-tauri/Cargo.toml` and confirm three new lines appeared under `[dependencies]`. Record the resolved versions — the API notes below were written against `rust_xlsxwriter` 0.7x and `calamine` 0.26; if cargo resolved something newer and a call below does not compile, check that crate's docs rather than guessing.

- [ ] **Step 2: Register the module**

Create `src-tauri/src/bulk/mod.rs` with just:

```rust
//! Bulk asset management through an Excel round trip.
//!
//! Three files with hard boundaries, because that boundary is what makes the
//! interesting logic testable without Postgres or Excel:
//!   sheet.rs  -- files only, never the database
//!   diff.rs   -- pure functions, neither files nor the database
//!   mod.rs    -- the only place that does both

pub mod diff;
pub mod sheet;
```

Create an empty `src-tauri/src/bulk/diff.rs` for now (Task 9 fills it):

```rust
// Filled in by the diff task.
```

Add `pub mod bulk;` to `src-tauri/src/lib.rs`, alphabetically first in the module list (before `pub mod blp_driver;`... check: `blp_driver` sorts before `budget` sorts before `bulk`; place it after `pub mod budget;`).

- [ ] **Step 3: Write the failing test**

Create `src-tauri/src/bulk/sheet.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<ExportRow> {
        vec![ExportRow {
            id: 7,
            label: "Apple".into(),
            class: "Equity".into(),
            id_kind: "ticker".into(),
            ticker: "AAPL US".into(),
            isin: String::new(),
            yellow_key: "Equity".into(),
            active: true,
            security: "AAPL US Equity".into(),
            views: vec!["Daily".into()],
        }]
    }

    #[test]
    fn writes_a_file_with_the_expected_header_and_one_column_per_view() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assets.xlsx");
        let views = vec!["Daily".to_string(), "Weekly".to_string()];
        write_assets_sheet(&path, &sample(), &views, &["Equity".to_string()]).unwrap();
        assert!(path.exists());
        assert!(std::fs::metadata(&path).unwrap().len() > 0);
    }

    #[test]
    fn hashing_the_same_bytes_twice_gives_the_same_digest() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.bin");
        std::fs::write(&path, b"hello").unwrap();
        let a = file_sha256(&path).unwrap();
        let b = file_sha256(&path).unwrap();
        assert_eq!(a, b);
        assert_eq!(a.len(), 64, "sha-256 hex is 64 characters");
        std::fs::write(&path, b"hello!").unwrap();
        assert_ne!(a, file_sha256(&path).unwrap());
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml bulk::sheet
```

Expected: compile error, `cannot find function 'write_assets_sheet'`.

- [ ] **Step 5: Write the implementation above the test module in `src-tauri/src/bulk/sheet.rs`**

```rust
//! Reading and writing the assets workbook. No database access lives here.

use crate::error::{AppError, AppResult};
use rust_xlsxwriter::{DataValidation, DataValidationRule, Format, Workbook};
use sha2::{Digest, Sha256};
use std::path::Path;

pub const SHEET_NAME: &str = "Assets";
pub const FIXED_HEADERS: [&str; 9] = [
    "id", "label", "class", "id_kind", "ticker", "isin", "yellow_key", "active", "security",
];

/// One asset as it appears in the exported workbook. `security` is written for
/// the reader's benefit only -- import always recomputes it.
#[derive(Debug, Clone, PartialEq)]
pub struct ExportRow {
    pub id: i64,
    pub label: String,
    pub class: String,
    pub id_kind: String,
    pub ticker: String,
    pub isin: String,
    pub yellow_key: String,
    pub active: bool,
    pub security: String,
    /// Names of the views this asset belongs to.
    pub views: Vec<String>,
}

fn xlsx_err(e: rust_xlsxwriter::XlsxError) -> AppError {
    AppError::Validation(format!("spreadsheet error: {e}"))
}

pub fn write_assets_sheet(
    path: &Path,
    rows: &[ExportRow],
    view_names: &[String],
    class_names: &[String],
) -> AppResult<()> {
    let mut book = Workbook::new();
    let sheet = book.add_worksheet();
    sheet.set_name(SHEET_NAME).map_err(xlsx_err)?;

    let header = Format::new().set_bold();
    let readonly = Format::new().set_background_color(0xEEEEEE);

    for (c, h) in FIXED_HEADERS.iter().enumerate() {
        sheet.write_string_with_format(0, c as u16, *h, &header).map_err(xlsx_err)?;
    }
    for (i, v) in view_names.iter().enumerate() {
        let c = (FIXED_HEADERS.len() + i) as u16;
        sheet.write_string_with_format(0, c, v, &header).map_err(xlsx_err)?;
    }
    // The header stays put while scrolling a few hundred rows.
    sheet.set_freeze_panes(1, 0).map_err(xlsx_err)?;

    for (i, r) in rows.iter().enumerate() {
        let row = (i + 1) as u32;
        sheet.write_number_with_format(row, 0, r.id as f64, &readonly).map_err(xlsx_err)?;
        sheet.write_string(row, 1, &r.label).map_err(xlsx_err)?;
        sheet.write_string(row, 2, &r.class).map_err(xlsx_err)?;
        sheet.write_string(row, 3, &r.id_kind).map_err(xlsx_err)?;
        sheet.write_string(row, 4, &r.ticker).map_err(xlsx_err)?;
        sheet.write_string(row, 5, &r.isin).map_err(xlsx_err)?;
        sheet.write_string(row, 6, &r.yellow_key).map_err(xlsx_err)?;
        sheet.write_string(row, 7, if r.active { "yes" } else { "no" }).map_err(xlsx_err)?;
        sheet.write_string_with_format(row, 8, &r.security, &readonly).map_err(xlsx_err)?;
        for (j, v) in view_names.iter().enumerate() {
            let c = (FIXED_HEADERS.len() + j) as u16;
            let mark = if r.views.iter().any(|x| x == v) { "x" } else { "" };
            sheet.write_string(row, c, mark).map_err(xlsx_err)?;
        }
    }

    // Dropdowns turn three of the most typo-prone columns into pick lists.
    let last = rows.len().max(1) as u32;
    let classes: Vec<&str> = class_names.iter().map(String::as_str).collect();
    let dv_class = DataValidation::new().allow_list_strings(&classes).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 2, last, 2, &dv_class).map_err(xlsx_err)?;
    let dv_kind = DataValidation::new()
        .allow_list_strings(&["ticker", "isin"]).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 3, last, 3, &dv_kind).map_err(xlsx_err)?;
    let dv_active = DataValidation::new()
        .allow_list_strings(&["yes", "no"]).map_err(xlsx_err)?;
    sheet.add_data_validation(1, 7, last, 7, &dv_active).map_err(xlsx_err)?;
    // `yellow_key` deliberately has no dropdown: the set is open-ended
    // (Equity, Corp, Index, Curncy, Comdty, Govt, ...) and constraining it
    // would block a legitimate key nobody thought to list.
    let _ = DataValidationRule::<i32>::EqualTo(0); // keep the import honest if unused

    book.save(path).map_err(xlsx_err)?;
    Ok(())
}

pub fn file_sha256(path: &Path) -> AppResult<String> {
    let bytes = std::fs::read(path)?;
    let mut h = Sha256::new();
    h.update(&bytes);
    Ok(format!("{:x}", h.finalize()))
}
```

If `DataValidationRule` turns out to be unused, delete that placeholder line and its `use` import rather than keeping dead code.

- [ ] **Step 6: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml bulk::sheet
```

Expected: `test result: ok. 2 passed`.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src/lib.rs src-tauri/src/bulk/
git commit -m "feat: write the assets workbook with a frozen header and dropdowns"
```

---

## Task 8: Sheet reader

**Files:**
- Modify: `src-tauri/src/bulk/sheet.rs` (append the reader)
- Test: unit tests inside `src-tauri/src/bulk/sheet.rs`

**Interfaces:**
- Consumes: `ExportRow`, `SHEET_NAME`, `FIXED_HEADERS`, `write_assets_sheet` from Task 7.
- Produces:
  - `pub struct SheetRow { pub row_number: u32, pub id: Option<i64>, pub label: String, pub class: String, pub id_kind: String, pub ticker: String, pub isin: String, pub yellow_key: String, pub active: bool, pub views: Vec<String> }`
  - `pub struct SheetData { pub has_id_column: bool, pub view_columns: Vec<String>, pub rows: Vec<SheetRow> }`
  - `pub fn read_assets_sheet(path: &Path) -> AppResult<SheetData>`

`has_id_column` is the load-bearing flag of the whole feature: spec §8.1 guardrail 1 says a sheet without it can never propose a removal.

- [ ] **Step 1: Write the failing test**

Append inside the existing `mod tests` in `src-tauri/src/bulk/sheet.rs`:

```rust
    #[test]
    fn round_trips_a_written_sheet() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("assets.xlsx");
        let views = vec!["Daily".to_string(), "Weekly".to_string()];
        write_assets_sheet(&path, &sample(), &views, &["Equity".to_string()]).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert!(data.has_id_column);
        assert_eq!(data.view_columns, views);
        assert_eq!(data.rows.len(), 1);
        let r = &data.rows[0];
        assert_eq!(r.row_number, 2, "spreadsheet rows are 1-based and row 1 is the header");
        assert_eq!(r.id, Some(7));
        assert_eq!(r.label, "Apple");
        assert_eq!(r.class, "Equity");
        assert_eq!(r.id_kind, "ticker");
        assert_eq!(r.ticker, "AAPL US");
        assert_eq!(r.yellow_key, "Equity");
        assert!(r.active);
        assert_eq!(r.views, vec!["Daily".to_string()]);
    }

    #[test]
    fn a_sheet_without_an_id_column_is_flagged() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pasted.xlsx");
        let mut book = Workbook::new();
        let s = book.add_worksheet();
        s.set_name(SHEET_NAME).unwrap();
        for (c, h) in ["label", "class", "id_kind", "ticker", "yellow_key"].iter().enumerate() {
            s.write_string(0, c as u16, *h).unwrap();
        }
        s.write_string(1, 0, "Microsoft").unwrap();
        s.write_string(1, 1, "Equity").unwrap();
        s.write_string(1, 2, "ticker").unwrap();
        s.write_string(1, 3, "MSFT US").unwrap();
        s.write_string(1, 4, "Equity").unwrap();
        book.save(&path).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert!(!data.has_id_column);
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].id, None);
        assert_eq!(data.rows[0].label, "Microsoft");
        assert!(data.rows[0].active, "a sheet with no `active` column means all active");
    }

    #[test]
    fn blank_rows_are_skipped_not_read_as_empty_assets() {
        use rust_xlsxwriter::Workbook;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gappy.xlsx");
        let mut book = Workbook::new();
        let s = book.add_worksheet();
        s.set_name(SHEET_NAME).unwrap();
        for (c, h) in FIXED_HEADERS.iter().enumerate() {
            s.write_string(0, c as u16, *h).unwrap();
        }
        s.write_string(3, 1, "Sparse").unwrap(); // rows 2 and 3 left entirely blank
        s.write_string(3, 2, "Equity").unwrap();
        book.save(&path).unwrap();

        let data = read_assets_sheet(&path).unwrap();
        assert_eq!(data.rows.len(), 1);
        assert_eq!(data.rows[0].row_number, 4);
        assert_eq!(data.rows[0].label, "Sparse");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml bulk::sheet
```

Expected: compile error, `cannot find function 'read_assets_sheet'`.

- [ ] **Step 3: Append the reader to `src-tauri/src/bulk/sheet.rs`**

```rust
use calamine::{Data, Reader, Xlsx};

/// One row as the user left it. Nothing here is validated or resolved: that is
/// the differ's job, so that validation can be unit-tested without a file.
#[derive(Debug, Clone, PartialEq)]
pub struct SheetRow {
    /// 1-based spreadsheet row number, so error messages match what Excel shows.
    pub row_number: u32,
    pub id: Option<i64>,
    pub label: String,
    pub class: String,
    pub id_kind: String,
    pub ticker: String,
    pub isin: String,
    pub yellow_key: String,
    pub active: bool,
    pub views: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SheetData {
    /// False for a hand-built or pasted list. Guardrail 1: such a file can
    /// never propose a removal, because the absence of a row means nothing
    /// when the file was never a full export in the first place.
    pub has_id_column: bool,
    pub view_columns: Vec<String>,
    pub rows: Vec<SheetRow>,
}

fn cell_text(row: &[Data], idx: Option<usize>) -> String {
    match idx.and_then(|i| row.get(i)) {
        None | Some(Data::Empty) => String::new(),
        Some(Data::Float(f)) => {
            // Excel stores every number as a float; an id typed as 12 arrives
            // as 12.0 and must not become the string "12.0".
            if f.fract() == 0.0 { format!("{}", *f as i64) } else { f.to_string() }
        }
        Some(Data::Int(i)) => i.to_string(),
        Some(Data::Bool(b)) => (if *b { "yes" } else { "no" }).to_string(),
        Some(other) => other.to_string(),
    }
    .trim()
    .to_string()
}

pub fn read_assets_sheet(path: &Path) -> AppResult<SheetData> {
    let mut book: Xlsx<_> = calamine::open_workbook(path)
        .map_err(|e| AppError::Validation(format!("cannot open {}: {e}", path.display())))?;
    let range = book.worksheet_range(SHEET_NAME).map_err(|e| {
        AppError::Validation(format!("workbook has no sheet named '{SHEET_NAME}': {e}"))
    })?;

    let mut rows_iter = range.rows();
    let header: Vec<String> = rows_iter
        .next()
        .ok_or_else(|| AppError::Validation("sheet is empty".into()))?
        .iter()
        .map(|c| c.to_string().trim().to_lowercase())
        .collect();

    let col = |name: &str| header.iter().position(|h| h == name);
    let (c_id, c_label, c_class, c_kind, c_ticker, c_isin, c_key, c_active) = (
        col("id"), col("label"), col("class"), col("id_kind"),
        col("ticker"), col("isin"), col("yellow_key"), col("active"),
    );
    if c_label.is_none() {
        return Err(AppError::Validation("sheet has no 'label' column".into()));
    }

    // Anything that is not a known fixed header is a view column. `security` is
    // a fixed header and is read but discarded -- import recomputes it.
    let view_columns: Vec<String> = range
        .rows()
        .next()
        .map(|r| r.iter().map(|c| c.to_string().trim().to_string()).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .enumerate()
        .filter(|(_, h)| !h.is_empty() && !FIXED_HEADERS.contains(&h.to_lowercase().as_str()))
        .map(|(_, h)| h)
        .collect();
    let view_indices: Vec<usize> = header
        .iter()
        .enumerate()
        .filter(|(_, h)| !h.is_empty() && !FIXED_HEADERS.contains(&h.as_str()))
        .map(|(i, _)| i)
        .collect();

    let mut rows = Vec::new();
    for (offset, r) in rows_iter.enumerate() {
        let row_number = (offset + 2) as u32; // header is row 1
        if r.iter().all(|c| matches!(c, Data::Empty)) {
            continue;
        }
        let id_text = cell_text(r, c_id);
        let id = if id_text.is_empty() {
            None
        } else {
            Some(id_text.parse::<i64>().map_err(|_| {
                AppError::Validation(format!("row {row_number}: id '{id_text}' is not a number"))
            })?)
        };
        let active_text = cell_text(r, c_active).to_lowercase();
        let active = match active_text.as_str() {
            "" => true, // no column, or blank: assume the asset stays collected
            "yes" | "y" | "true" | "1" | "oui" => true,
            _ => false,
        };
        let views = view_indices
            .iter()
            .zip(view_columns.iter())
            .filter(|(i, _)| !cell_text(r, Some(**i)).is_empty())
            .map(|(_, name)| name.clone())
            .collect();

        rows.push(SheetRow {
            row_number,
            id,
            label: cell_text(r, c_label),
            class: cell_text(r, c_class),
            id_kind: cell_text(r, c_kind).to_lowercase(),
            ticker: cell_text(r, c_ticker),
            isin: cell_text(r, c_isin),
            yellow_key: cell_text(r, c_key),
            active,
            views,
        });
    }

    Ok(SheetData { has_id_column: c_id.is_some(), view_columns, rows })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml bulk::sheet
```

Expected: `test result: ok. 5 passed`.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/bulk/sheet.rs
git commit -m "feat: read the assets workbook, flagging sheets that carry no id column"
```

---

## Task 9: The differ, as a pure function

**Files:**
- Modify: `src-tauri/src/bulk/diff.rs` (replace the placeholder)
- Test: unit tests inside `src-tauri/src/bulk/diff.rs`

**Interfaces:**
- Consumes: `SheetData` and `SheetRow` from Task 8; `crate::registry::resolve_bdp_security`.
- Produces:
  - `pub struct DbAsset { pub id: i64, pub label: String, pub class: String, pub id_kind: String, pub ticker: String, pub isin: String, pub yellow_key: String, pub active: bool, pub bdp_security: String, pub views: Vec<String> }`
  - `pub struct AssetRef { pub id: i64, pub label: String, pub security: String }`
  - `pub struct AddRow { pub row_number: u32, pub label: String, pub class: String, pub id_kind: String, pub ticker: String, pub isin: String, pub yellow_key: String, pub active: bool, pub security: String, pub views: Vec<String> }`
  - `pub struct EditRow { pub id: i64, pub row_number: u32, pub label: String, pub class: String, pub id_kind: String, pub ticker: String, pub isin: String, pub yellow_key: String, pub security: String, pub changed: Vec<String> }`
  - `pub struct MembershipChange { pub id: i64, pub label: String, pub added: Vec<String>, pub removed: Vec<String> }`
  - `pub struct InvalidRow { pub row_number: u32, pub reason: String }`
  - `pub struct ImportPlan { pub file_hash: String, pub has_id_column: bool, pub adds: Vec<AddRow>, pub edits: Vec<EditRow>, pub retires: Vec<AssetRef>, pub reactivations: Vec<AssetRef>, pub membership_changes: Vec<MembershipChange>, pub removals: Vec<AssetRef>, pub invalid_rows: Vec<InvalidRow>, pub active_asset_count: i64, pub requires_typed_confirmation: bool }`
  - `pub fn diff(sheet: &SheetData, db: &[DbAsset], known_classes: &[String], known_views: &[String], file_hash: &str) -> ImportPlan`

This is the task that justifies the `bulk/` directory. **`diff.rs` must not import `sqlx`, `std::fs`, `calamine`, or `rust_xlsxwriter`.**

- [ ] **Step 1: Write the failing tests**

Replace the contents of `src-tauri/src/bulk/diff.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::bulk::sheet::{SheetData, SheetRow};

    fn classes() -> Vec<String> { vec!["Equity".into(), "Corp".into()] }
    fn views() -> Vec<String> { vec!["Daily".into(), "Weekly".into()] }

    fn db_apple() -> DbAsset {
        DbAsset {
            id: 1, label: "Apple".into(), class: "Equity".into(), id_kind: "ticker".into(),
            ticker: "AAPL US".into(), isin: String::new(), yellow_key: "Equity".into(),
            active: true, bdp_security: "AAPL US Equity".into(), views: vec!["Daily".into()],
        }
    }

    fn row_from(a: &DbAsset) -> SheetRow {
        SheetRow {
            row_number: 2, id: Some(a.id), label: a.label.clone(), class: a.class.clone(),
            id_kind: a.id_kind.clone(), ticker: a.ticker.clone(), isin: a.isin.clone(),
            yellow_key: a.yellow_key.clone(), active: a.active, views: a.views.clone(),
        }
    }

    fn sheet(rows: Vec<SheetRow>, has_id: bool) -> SheetData {
        SheetData { has_id_column: has_id, view_columns: views(), rows }
    }

    #[test]
    fn an_unchanged_export_produces_an_empty_plan() {
        let db = vec![db_apple()];
        let s = sheet(vec![row_from(&db[0])], true);
        let p = diff(&s, &db, &classes(), &views(), "hash");
        assert!(p.adds.is_empty() && p.edits.is_empty() && p.removals.is_empty()
                && p.retires.is_empty() && p.reactivations.is_empty()
                && p.membership_changes.is_empty() && p.invalid_rows.is_empty(),
                "round trip must be a no-op, got {p:?}");
    }

    #[test]
    fn a_blank_id_is_an_add_with_a_resolved_security() {
        let db = vec![db_apple()];
        let mut new_row = row_from(&db[0]);
        new_row.id = None;
        new_row.row_number = 3;
        new_row.label = "Microsoft".into();
        new_row.ticker = "MSFT US".into();
        new_row.views = vec!["Weekly".into()];
        let p = diff(&sheet(vec![row_from(&db[0]), new_row], true), &db,
                     &classes(), &views(), "hash");
        assert_eq!(p.adds.len(), 1);
        assert_eq!(p.adds[0].label, "Microsoft");
        assert_eq!(p.adds[0].security, "MSFT US Equity");
        assert_eq!(p.adds[0].views, vec!["Weekly".to_string()]);
        assert!(p.removals.is_empty());
    }

    #[test]
    fn identity_travels_in_the_id_so_a_rename_is_an_edit() {
        let db = vec![db_apple()];
        let mut r = row_from(&db[0]);
        r.label = "Apple Inc".into();
        r.ticker = "AAPL UW".into();
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert!(p.adds.is_empty() && p.removals.is_empty());
        assert_eq!(p.edits.len(), 1);
        assert_eq!(p.edits[0].id, 1);
        assert_eq!(p.edits[0].security, "AAPL UW Equity");
        assert!(p.edits[0].changed.contains(&"label".to_string()));
        assert!(p.edits[0].changed.contains(&"ticker".to_string()));
    }

    #[test]
    fn flipping_active_is_a_retire_not_an_edit() {
        let db = vec![db_apple()];
        let mut r = row_from(&db[0]);
        r.active = false;
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert!(p.edits.is_empty(), "active is its own category");
        assert_eq!(p.retires.len(), 1);
        assert_eq!(p.retires[0].id, 1);
    }

    #[test]
    fn flipping_active_back_on_is_a_reactivation() {
        let mut a = db_apple();
        a.active = false;
        let db = vec![a.clone()];
        let mut r = row_from(&a);
        r.active = true;
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.reactivations.len(), 1);
        assert!(p.retires.is_empty());
    }

    #[test]
    fn view_marks_become_membership_changes() {
        let db = vec![db_apple()];
        let mut r = row_from(&db[0]);
        r.views = vec!["Weekly".into()]; // was Daily
        let p = diff(&sheet(vec![r], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.membership_changes.len(), 1);
        assert_eq!(p.membership_changes[0].added, vec!["Weekly".to_string()]);
        assert_eq!(p.membership_changes[0].removed, vec!["Daily".to_string()]);
    }

    #[test]
    fn a_missing_row_is_a_removal_when_the_sheet_has_an_id_column() {
        let db = vec![db_apple()];
        let p = diff(&sheet(vec![], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.removals.len(), 1);
        assert_eq!(p.removals[0].id, 1);
    }

    /// Guardrail 1, spec §8.1 -- the one that makes pasted lists safe.
    #[test]
    fn a_sheet_without_an_id_column_never_proposes_a_removal() {
        let db = vec![db_apple()];
        let pasted = SheetRow {
            row_number: 2, id: None, label: "Microsoft".into(), class: "Equity".into(),
            id_kind: "ticker".into(), ticker: "MSFT US".into(), isin: String::new(),
            yellow_key: "Equity".into(), active: true, views: vec![],
        };
        let p = diff(&sheet(vec![pasted], false), &db, &classes(), &views(), "hash");
        assert!(p.removals.is_empty(), "a pasted list must not delete the book");
        assert_eq!(p.adds.len(), 1);
    }

    /// Guardrail 2, spec §8.1.
    #[test]
    fn removing_more_than_half_the_active_book_demands_typed_confirmation() {
        let db: Vec<DbAsset> = (1..=4).map(|i| {
            let mut a = db_apple();
            a.id = i;
            a.label = format!("A{i}");
            a.bdp_security = format!("A{i} US Equity");
            a.ticker = format!("A{i} US");
            a
        }).collect();
        let kept = SheetRow { row_number: 2, ..row_from(&db[0]) };
        let p = diff(&sheet(vec![kept], true), &db, &classes(), &views(), "hash");
        assert_eq!(p.removals.len(), 3);
        assert_eq!(p.active_asset_count, 4);
        assert!(p.requires_typed_confirmation);

        // Two of four is not "more than half".
        let two = vec![SheetRow { row_number: 2, ..row_from(&db[0]) },
                       SheetRow { row_number: 3, ..row_from(&db[1]) },
                       SheetRow { row_number: 4, ..row_from(&db[2]) }];
        let q = diff(&sheet(two, true), &db, &classes(), &views(), "hash");
        assert_eq!(q.removals.len(), 1);
        assert!(!q.requires_typed_confirmation);
    }

    #[test]
    fn unknown_class_unknown_id_and_bad_identifier_are_invalid_rows() {
        let db = vec![db_apple()];

        let mut bad_class = row_from(&db[0]);
        bad_class.class = "Nonexistent".into();
        assert_eq!(diff(&sheet(vec![bad_class], true), &db, &classes(), &views(), "h")
                       .invalid_rows.len(), 1);

        let mut bad_id = row_from(&db[0]);
        bad_id.id = Some(999);
        let p = diff(&sheet(vec![bad_id], true), &db, &classes(), &views(), "h");
        assert_eq!(p.invalid_rows.len(), 1);
        assert!(p.invalid_rows[0].reason.contains("999"));

        // id_kind says ticker but only the isin column is filled.
        let mut mismatch = row_from(&db[0]);
        mismatch.ticker = String::new();
        mismatch.isin = "FR0000120271".into();
        assert_eq!(diff(&sheet(vec![mismatch], true), &db, &classes(), &views(), "h")
                       .invalid_rows.len(), 1);
    }

    #[test]
    fn a_duplicate_id_and_a_colliding_security_are_both_rejected() {
        let mut msft = db_apple();
        msft.id = 2;
        msft.label = "Microsoft".into();
        msft.ticker = "MSFT US".into();
        msft.bdp_security = "MSFT US Equity".into();
        let db = vec![db_apple(), msft];

        let dup = vec![row_from(&db[0]), SheetRow { row_number: 3, ..row_from(&db[0]) }];
        let p = diff(&sheet(dup, true), &db, &classes(), &views(), "h");
        assert!(p.invalid_rows.iter().any(|i| i.reason.contains("twice")),
                "got {:?}", p.invalid_rows);

        // Renaming Apple onto Microsoft's security must not reach the UNIQUE index.
        let mut collide = row_from(&db[0]);
        collide.ticker = "MSFT US".into();
        let q = diff(&sheet(vec![collide, SheetRow { row_number: 3, ..row_from(&db[1]) }], true),
                     &db, &classes(), &views(), "h");
        assert!(q.invalid_rows.iter().any(|i| i.reason.contains("MSFT US Equity")),
                "got {:?}", q.invalid_rows);
    }

    #[test]
    fn a_view_column_naming_an_unknown_view_is_reported_against_the_header() {
        let db = vec![db_apple()];
        let s = SheetData {
            has_id_column: true,
            view_columns: vec!["Daily".into(), "Ghost".into()],
            rows: vec![row_from(&db[0])],
        };
        let p = diff(&s, &db, &classes(), &views(), "h");
        assert_eq!(p.invalid_rows.len(), 1);
        assert_eq!(p.invalid_rows[0].row_number, 1, "header problems belong to row 1");
        assert!(p.invalid_rows[0].reason.contains("Ghost"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml bulk::diff
```

Expected: compile error, `cannot find function 'diff'`.

- [ ] **Step 3: Write the implementation above the test module in `src-tauri/src/bulk/diff.rs`**

```rust
//! Diffing a parsed sheet against the registry.
//!
//! This file is deliberately pure: no database, no filesystem, no spreadsheet
//! crate. Every interesting decision in the bulk import -- what counts as an
//! edit, when a missing row is a removal, which rows are invalid -- is decided
//! here and therefore testable in milliseconds without Postgres or Excel.

use crate::bulk::sheet::SheetData;
use crate::registry::resolve_bdp_security;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// An asset as the database currently holds it, flattened to names so the
/// differ never has to resolve an id to a class or a view.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DbAsset {
    pub id: i64,
    pub label: String,
    pub class: String,
    pub id_kind: String,
    pub ticker: String,
    pub isin: String,
    pub yellow_key: String,
    pub active: bool,
    pub bdp_security: String,
    pub views: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetRef {
    pub id: i64,
    pub label: String,
    pub security: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddRow {
    pub row_number: u32,
    pub label: String,
    pub class: String,
    pub id_kind: String,
    pub ticker: String,
    pub isin: String,
    pub yellow_key: String,
    pub active: bool,
    /// Resolved here so the apply step never re-derives it differently.
    pub security: String,
    pub views: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EditRow {
    pub id: i64,
    pub row_number: u32,
    pub label: String,
    pub class: String,
    pub id_kind: String,
    pub ticker: String,
    pub isin: String,
    pub yellow_key: String,
    pub security: String,
    /// Column names that differ from the database, for display.
    pub changed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MembershipChange {
    pub id: i64,
    pub label: String,
    pub added: Vec<String>,
    pub removed: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InvalidRow {
    pub row_number: u32,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportPlan {
    pub file_hash: String,
    pub has_id_column: bool,
    pub adds: Vec<AddRow>,
    pub edits: Vec<EditRow>,
    pub retires: Vec<AssetRef>,
    pub reactivations: Vec<AssetRef>,
    pub membership_changes: Vec<MembershipChange>,
    pub removals: Vec<AssetRef>,
    pub invalid_rows: Vec<InvalidRow>,
    pub active_asset_count: i64,
    pub requires_typed_confirmation: bool,
}

impl ImportPlan {
    pub fn is_empty(&self) -> bool {
        self.adds.is_empty()
            && self.edits.is_empty()
            && self.retires.is_empty()
            && self.reactivations.is_empty()
            && self.membership_changes.is_empty()
            && self.removals.is_empty()
    }
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

pub fn diff(
    sheet: &SheetData,
    db: &[DbAsset],
    known_classes: &[String],
    known_views: &[String],
    file_hash: &str,
) -> ImportPlan {
    let mut plan = ImportPlan {
        file_hash: file_hash.to_string(),
        has_id_column: sheet.has_id_column,
        adds: vec![],
        edits: vec![],
        retires: vec![],
        reactivations: vec![],
        membership_changes: vec![],
        removals: vec![],
        invalid_rows: vec![],
        active_asset_count: db.iter().filter(|a| a.active).count() as i64,
        requires_typed_confirmation: false,
    };

    let classes: HashSet<&str> = known_classes.iter().map(String::as_str).collect();
    let views: HashSet<&str> = known_views.iter().map(String::as_str).collect();
    let by_id: HashMap<i64, &DbAsset> = db.iter().map(|a| (a.id, a)).collect();

    // Header problems are attributed to row 1, which is where Excel shows them.
    for v in &sheet.view_columns {
        if !views.contains(v.as_str()) {
            plan.invalid_rows.push(InvalidRow {
                row_number: 1,
                reason: format!("column '{v}' names a view that does not exist"),
            });
        }
    }

    // Securities claimed by rows in this sheet, used to catch a rename that
    // would hit UNIQUE (bdp_security) before the transaction ever opens.
    let mut claimed: HashMap<String, u32> = HashMap::new();
    let mut seen_ids: HashSet<i64> = HashSet::new();
    let mut present_ids: HashSet<i64> = HashSet::new();

    for r in &sheet.rows {
        if r.label.is_empty() {
            plan.invalid_rows.push(InvalidRow {
                row_number: r.row_number,
                reason: "label is empty".into(),
            });
            continue;
        }
        if !classes.contains(r.class.as_str()) {
            plan.invalid_rows.push(InvalidRow {
                row_number: r.row_number,
                reason: format!("class '{}' does not exist", r.class),
            });
            continue;
        }
        let ticker = (!r.ticker.is_empty()).then_some(r.ticker.as_str());
        let isin = (!r.isin.is_empty()).then_some(r.isin.as_str());
        let security = match resolve_bdp_security(&r.id_kind, ticker, isin, &r.yellow_key) {
            Ok(s) => s,
            Err(e) => {
                plan.invalid_rows.push(InvalidRow {
                    row_number: r.row_number,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        if let Some(first) = claimed.get(&security) {
            plan.invalid_rows.push(InvalidRow {
                row_number: r.row_number,
                reason: format!("security '{security}' is already claimed by row {first}"),
            });
            continue;
        }
        claimed.insert(security.clone(), r.row_number);

        // A security that belongs to a DIFFERENT asset would violate the unique
        // index. The same asset keeping its own security is fine.
        if let Some(owner) = db.iter().find(|a| a.bdp_security == security) {
            if Some(owner.id) != r.id {
                plan.invalid_rows.push(InvalidRow {
                    row_number: r.row_number,
                    reason: format!(
                        "security '{security}' already belongs to '{}'", owner.label),
                });
                continue;
            }
        }

        let views_now = sorted(r.views.clone());

        match r.id {
            None => plan.adds.push(AddRow {
                row_number: r.row_number,
                label: r.label.clone(),
                class: r.class.clone(),
                id_kind: r.id_kind.clone(),
                ticker: r.ticker.clone(),
                isin: r.isin.clone(),
                yellow_key: r.yellow_key.clone(),
                active: r.active,
                security,
                views: views_now,
            }),
            Some(id) => {
                if !seen_ids.insert(id) {
                    plan.invalid_rows.push(InvalidRow {
                        row_number: r.row_number,
                        reason: format!("id {id} appears twice in the sheet"),
                    });
                    continue;
                }
                let Some(cur) = by_id.get(&id) else {
                    plan.invalid_rows.push(InvalidRow {
                        row_number: r.row_number,
                        reason: format!("id {id} is not in the database"),
                    });
                    continue;
                };
                present_ids.insert(id);

                let mut changed = Vec::new();
                if r.label != cur.label { changed.push("label".to_string()); }
                if r.class != cur.class { changed.push("class".to_string()); }
                if r.id_kind != cur.id_kind { changed.push("id_kind".to_string()); }
                if r.ticker != cur.ticker { changed.push("ticker".to_string()); }
                if r.isin != cur.isin { changed.push("isin".to_string()); }
                if r.yellow_key != cur.yellow_key { changed.push("yellow_key".to_string()); }
                if !changed.is_empty() {
                    plan.edits.push(EditRow {
                        id,
                        row_number: r.row_number,
                        label: r.label.clone(),
                        class: r.class.clone(),
                        id_kind: r.id_kind.clone(),
                        ticker: r.ticker.clone(),
                        isin: r.isin.clone(),
                        yellow_key: r.yellow_key.clone(),
                        security: security.clone(),
                        changed,
                    });
                }

                // `active` is its own category, never an edit: the two ways of
                // stopping collection should read as one thing in the diff.
                let aref = AssetRef { id, label: cur.label.clone(), security };
                if r.active != cur.active {
                    if r.active { plan.reactivations.push(aref); }
                    else { plan.retires.push(aref); }
                }

                let before = sorted(cur.views.clone());
                if views_now != before {
                    plan.membership_changes.push(MembershipChange {
                        id,
                        label: cur.label.clone(),
                        added: views_now.iter().filter(|v| !before.contains(v))
                            .cloned().collect(),
                        removed: before.iter().filter(|v| !views_now.contains(v))
                            .cloned().collect(),
                    });
                }
            }
        }
    }

    // Guardrail 1: only a file that came from Export -- one carrying ids -- can
    // say that a missing row means "remove this".
    if sheet.has_id_column {
        for a in db {
            if !present_ids.contains(&a.id) {
                plan.removals.push(AssetRef {
                    id: a.id,
                    label: a.label.clone(),
                    security: a.bdp_security.clone(),
                });
            }
        }
    }

    // Guardrail 2: a removal set larger than half the active book is more
    // likely a truncated paste than an intention.
    plan.requires_typed_confirmation =
        plan.active_asset_count > 0
            && (plan.removals.len() as i64) * 2 > plan.active_asset_count;

    plan
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml bulk::diff
```

Expected: `test result: ok. 12 passed`.

- [ ] **Step 5: Verify the purity constraint by inspection**

```bash
grep -nE "use (sqlx|std::fs|calamine|rust_xlsxwriter)" src-tauri/src/bulk/diff.rs
```

Expected: no output. If anything matches, the design boundary is broken — move that code to `sheet.rs` or `mod.rs`.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/bulk/diff.rs
git commit -m "feat: pure differ turning a sheet plus the registry into an import plan"
```

---

## Task 10: Export, preview and apply against the database

**Files:**
- Modify: `src-tauri/src/bulk/mod.rs` (replace the stub with the real module)
- Test: `src-tauri/tests/db_integration.rs` (append)

**Interfaces:**
- Consumes: `sheet::{ExportRow, SheetData, read_assets_sheet, write_assets_sheet, file_sha256}`; `diff::{DbAsset, ImportPlan, diff}`; `deletion::{DeleteMode, purge_asset_tx}`; `registry::resolve_bdp_security`.
- Produces:
  - `pub struct ImportResult { pub added: i64, pub edited: i64, pub retired: i64, pub reactivated: i64, pub membership_updated: i64, pub removed: i64 }`
  - `pub async fn load_db_assets(pool: &PgPool) -> AppResult<Vec<DbAsset>>`
  - `pub async fn export_assets_xlsx(pool: &PgPool, path: &Path) -> AppResult<()>`
  - `pub async fn preview_import(pool: &PgPool, path: &Path) -> AppResult<ImportPlan>`
  - `pub async fn apply_import(pool: &PgPool, path: &Path, file_hash: &str, removal_modes: &[(i64, DeleteMode)], confirmed_removal_count: Option<i64>) -> AppResult<ImportResult>`

- [ ] **Step 1: Write the failing test**

Append to `src-tauri/tests/db_integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn export_then_import_is_a_no_op() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("RoundTripCls"), "test").await.unwrap();
    getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id,
            label: "Round Trip".into(),
            id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("RTP"))),
            isin: None,
            yellow_key: "Equity".into(),
        }).await.unwrap();

    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();
    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert!(plan.invalid_rows.is_empty(), "invalid rows: {:?}", plan.invalid_rows);
    assert!(plan.is_empty(), "a fresh round trip must change nothing, got {plan:?}");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_adds_edits_and_purges_in_one_transaction() {
    use getbloomdata_lib::deletion::DeleteMode;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    let class = getbloomdata_lib::registry::create_asset_class(
        &pool, &uniq("ApplyCls"), "test").await.unwrap();
    let keep = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id, label: "Keep".into(), id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("KEP"))), isin: None,
            yellow_key: "Equity".into() }).await.unwrap();
    let drop = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id, label: "Drop".into(), id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("DRP"))), isin: None,
            yellow_key: "Equity".into() }).await.unwrap();
    let third = getbloomdata_lib::registry::create_asset(
        &pool, getbloomdata_lib::registry::NewAsset {
            asset_class_id: class.id, label: "Third".into(), id_kind: "ticker".into(),
            ticker: Some(format!("{} US", uniq("THR"))), isin: None,
            yellow_key: "Equity".into() }).await.unwrap();

    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();

    // Rewrite the sheet: rename `keep`, add one, and leave `drop` out.
    // Other assets in the shared test database are untouched only because the
    // export carries them all; we edit the file rather than rebuilding it.
    let data = getbloomdata_lib::bulk::sheet::read_assets_sheet(&path).unwrap();
    let mut rows: Vec<getbloomdata_lib::bulk::sheet::ExportRow> = data.rows.iter()
        .filter(|r| r.id != Some(drop.id))
        .map(|r| getbloomdata_lib::bulk::sheet::ExportRow {
            id: r.id.unwrap_or(0),
            label: if r.id == Some(keep.id) { "Keep Renamed".into() } else { r.label.clone() },
            class: r.class.clone(), id_kind: r.id_kind.clone(),
            ticker: r.ticker.clone(), isin: r.isin.clone(),
            yellow_key: r.yellow_key.clone(), active: r.active,
            security: String::new(), views: r.views.clone(),
        }).collect();
    let new_ticker = format!("{} US", uniq("NEW"));
    rows.push(getbloomdata_lib::bulk::sheet::ExportRow {
        id: 0, label: "Brand New".into(), class: class.name.clone(),
        id_kind: "ticker".into(), ticker: new_ticker.clone(), isin: String::new(),
        yellow_key: "Equity".into(), active: true, security: String::new(), views: vec![],
    });
    let views: Vec<String> = getbloomdata_lib::views::list_views(&pool).await.unwrap()
        .into_iter().map(|v| v.name).collect();
    let classes: Vec<String> = getbloomdata_lib::registry::list_asset_classes(&pool).await
        .unwrap().into_iter().map(|c| c.name).collect();
    // id 0 means "blank" to the writer: write it as an add.
    getbloomdata_lib::bulk::sheet::write_assets_sheet(&path, &rows, &views, &classes).unwrap();

    let plan = getbloomdata_lib::bulk::preview_import(&pool, &path).await.unwrap();
    assert!(plan.invalid_rows.is_empty(), "invalid rows: {:?}", plan.invalid_rows);
    assert!(plan.removals.iter().any(|r| r.id == drop.id));
    assert!(plan.edits.iter().any(|e| e.id == keep.id));
    assert!(plan.adds.iter().any(|a| a.label == "Brand New"));

    let res = getbloomdata_lib::bulk::apply_import(
        &pool, &path, &plan.file_hash,
        &[(drop.id, DeleteMode::Purge)],
        Some(plan.removals.len() as i64)).await.unwrap();
    assert!(res.added >= 1 && res.edited >= 1 && res.removed >= 1);

    let (gone,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset WHERE id = $1")
        .bind(drop.id).fetch_one(&pool).await.unwrap();
    assert_eq!(gone, 0);
    let (label,): (String,) = sqlx::query_as("SELECT label FROM asset WHERE id = $1")
        .bind(keep.id).fetch_one(&pool).await.unwrap();
    assert_eq!(label, "Keep Renamed");
    let (still,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM asset WHERE id = $1")
        .bind(third.id).fetch_one(&pool).await.unwrap();
    assert_eq!(still, 1, "an untouched row must survive");
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn apply_import_refuses_a_stale_hash() {
    use getbloomdata_lib::error::AppError;
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("assets.xlsx");

    getbloomdata_lib::bulk::export_assets_xlsx(&pool, &path).await.unwrap();
    let err = getbloomdata_lib::bulk::apply_import(
        &pool, &path, "0000000000000000000000000000000000000000000000000000000000000000",
        &[], None).await.unwrap_err();
    assert!(matches!(err, AppError::ImportRejected { .. }), "got {err:?}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration export_then_import apply_import -- --ignored --nocapture
```

Expected: compile error, `cannot find function 'export_assets_xlsx'`.

- [ ] **Step 3: Replace `src-tauri/src/bulk/mod.rs` with the full module**

```rust
//! Bulk asset management through an Excel round trip.
//!
//! Three files with hard boundaries, because that boundary is what makes the
//! interesting logic testable without Postgres or Excel:
//!   sheet.rs  -- files only, never the database
//!   diff.rs   -- pure functions, neither files nor the database
//!   mod.rs    -- the only place that does both

pub mod diff;
pub mod sheet;

use crate::deletion::{purge_asset_tx, DeleteMode};
use crate::error::{AppError, AppResult};
use crate::registry::resolve_bdp_security;
use diff::{DbAsset, ImportPlan};
use serde::{Deserialize, Serialize};
use sheet::ExportRow;
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportResult {
    pub added: i64,
    pub edited: i64,
    pub retired: i64,
    pub reactivated: i64,
    pub membership_updated: i64,
    pub removed: i64,
}

/// Every asset, flattened to the names the sheet and the differ speak in.
/// Inactive assets are included: the sheet is the whole registry, and an
/// `active` of "no" is how the user sees a retired name.
pub async fn load_db_assets(pool: &PgPool) -> AppResult<Vec<DbAsset>> {
    let rows: Vec<(i64, String, String, String, Option<String>, Option<String>,
                   String, bool, String)> = sqlx::query_as(
        "SELECT a.id, a.label, c.name, a.id_kind, a.ticker, a.isin,
                a.yellow_key, a.active, a.bdp_security
         FROM asset a JOIN asset_class c ON c.id = a.asset_class_id
         ORDER BY a.label")
        .fetch_all(pool).await?;

    let memberships: Vec<(i64, String)> = sqlx::query_as(
        "SELECT va.asset_id, v.name FROM view_asset va JOIN view v ON v.id = va.view_id")
        .fetch_all(pool).await?;
    let mut by_asset: HashMap<i64, Vec<String>> = HashMap::new();
    for (aid, name) in memberships {
        by_asset.entry(aid).or_default().push(name);
    }

    Ok(rows.into_iter().map(|(id, label, class, id_kind, ticker, isin,
                              yellow_key, active, bdp_security)| DbAsset {
        id, label, class, id_kind,
        ticker: ticker.unwrap_or_default(),
        isin: isin.unwrap_or_default(),
        yellow_key, active, bdp_security,
        views: by_asset.remove(&id).unwrap_or_default(),
    }).collect())
}

async fn view_names(pool: &PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM view ORDER BY name")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

async fn class_names(pool: &PgPool) -> AppResult<Vec<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT name FROM asset_class ORDER BY name")
        .fetch_all(pool).await?;
    Ok(rows.into_iter().map(|(n,)| n).collect())
}

pub async fn export_assets_xlsx(pool: &PgPool, path: &Path) -> AppResult<()> {
    let assets = load_db_assets(pool).await?;
    let views = view_names(pool).await?;
    let classes = class_names(pool).await?;
    let rows: Vec<ExportRow> = assets.into_iter().map(|a| ExportRow {
        id: a.id, label: a.label, class: a.class, id_kind: a.id_kind,
        ticker: a.ticker, isin: a.isin, yellow_key: a.yellow_key,
        active: a.active, security: a.bdp_security, views: a.views,
    }).collect();
    sheet::write_assets_sheet(path, &rows, &views, &classes)
}

async fn plan_for(pool: &PgPool, path: &Path) -> AppResult<ImportPlan> {
    let data = sheet::read_assets_sheet(path)?;
    let hash = sheet::file_sha256(path)?;
    let db = load_db_assets(pool).await?;
    let views = view_names(pool).await?;
    let classes = class_names(pool).await?;
    Ok(diff::diff(&data, &db, &classes, &views, &hash))
}

pub async fn preview_import(pool: &PgPool, path: &Path) -> AppResult<ImportPlan> {
    plan_for(pool, path).await
}

/// Re-reads and re-diffs the file, then applies everything or nothing.
///
/// The hash check is the point of the two phases: a plan the user reviewed can
/// never be applied against a file that changed underneath it. The re-diff
/// matters just as much -- the database may have moved on even when the file
/// has not.
pub async fn apply_import(
    pool: &PgPool,
    path: &Path,
    file_hash: &str,
    removal_modes: &[(i64, DeleteMode)],
    confirmed_removal_count: Option<i64>,
) -> AppResult<ImportResult> {
    let actual = sheet::file_sha256(path)?;
    if actual != file_hash {
        return Err(AppError::ImportRejected {
            reason: "the file changed since it was previewed; preview it again".into(),
        });
    }
    let plan = plan_for(pool, path).await?;
    if !plan.invalid_rows.is_empty() {
        return Err(AppError::ImportRejected {
            reason: format!("{} invalid row(s); nothing was applied", plan.invalid_rows.len()),
        });
    }
    if plan.requires_typed_confirmation
        && confirmed_removal_count != Some(plan.removals.len() as i64)
    {
        return Err(AppError::ImportRejected {
            reason: format!(
                "this removes {} of {} active assets; confirm the count to proceed",
                plan.removals.len(), plan.active_asset_count),
        });
    }

    let modes: HashMap<i64, DeleteMode> = removal_modes.iter().copied().collect();
    let classes: HashMap<String, i64> = {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM asset_class")
            .fetch_all(pool).await?;
        rows.into_iter().map(|(id, n)| (n, id)).collect()
    };
    let views: HashMap<String, i64> = {
        let rows: Vec<(i64, String)> = sqlx::query_as("SELECT id, name FROM view")
            .fetch_all(pool).await?;
        rows.into_iter().map(|(id, n)| (n, id)).collect()
    };
    let class_id = |name: &str| -> AppResult<i64> {
        classes.get(name).copied()
            .ok_or_else(|| AppError::ImportRejected { reason: format!("no class '{name}'") })
    };
    let view_id = |name: &str| -> AppResult<i64> {
        views.get(name).copied()
            .ok_or_else(|| AppError::ImportRejected { reason: format!("no view '{name}'") })
    };

    let mut res = ImportResult::default();
    let mut tx = pool.begin().await?;

    for a in &plan.adds {
        // The security is recomputed here rather than trusted from the plan:
        // this is the last line of defence for the doubled-yellow-key fault.
        let sec = resolve_bdp_security(
            &a.id_kind,
            (!a.ticker.is_empty()).then_some(a.ticker.as_str()),
            (!a.isin.is_empty()).then_some(a.isin.as_str()),
            &a.yellow_key)?;
        let (new_id,): (i64,) = sqlx::query_as(
            "INSERT INTO asset (asset_class_id, label, id_kind, ticker, isin,
                                yellow_key, bdp_security, active)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8) RETURNING id")
            .bind(class_id(&a.class)?).bind(&a.label).bind(&a.id_kind)
            .bind((!a.ticker.is_empty()).then(|| a.ticker.clone()))
            .bind((!a.isin.is_empty()).then(|| a.isin.clone()))
            .bind(a.yellow_key.trim()).bind(&sec).bind(a.active)
            .fetch_one(&mut *tx).await?;
        for v in &a.views {
            sqlx::query("INSERT INTO view_asset (view_id, asset_id) VALUES ($1,$2)")
                .bind(view_id(v)?).bind(new_id).execute(&mut *tx).await?;
        }
        res.added += 1;
    }

    for e in &plan.edits {
        let sec = resolve_bdp_security(
            &e.id_kind,
            (!e.ticker.is_empty()).then_some(e.ticker.as_str()),
            (!e.isin.is_empty()).then_some(e.isin.as_str()),
            &e.yellow_key)?;
        sqlx::query(
            "UPDATE asset SET asset_class_id = $2, label = $3, id_kind = $4,
                              ticker = $5, isin = $6, yellow_key = $7, bdp_security = $8
             WHERE id = $1")
            .bind(e.id).bind(class_id(&e.class)?).bind(&e.label).bind(&e.id_kind)
            .bind((!e.ticker.is_empty()).then(|| e.ticker.clone()))
            .bind((!e.isin.is_empty()).then(|| e.isin.clone()))
            .bind(e.yellow_key.trim()).bind(&sec)
            .execute(&mut *tx).await?;
        res.edited += 1;
    }

    for m in &plan.membership_changes {
        for v in &m.added {
            sqlx::query(
                "INSERT INTO view_asset (view_id, asset_id) VALUES ($1,$2)
                 ON CONFLICT DO NOTHING")
                .bind(view_id(v)?).bind(m.id).execute(&mut *tx).await?;
        }
        for v in &m.removed {
            sqlx::query("DELETE FROM view_asset WHERE view_id = $1 AND asset_id = $2")
                .bind(view_id(v)?).bind(m.id).execute(&mut *tx).await?;
        }
        res.membership_updated += 1;
    }

    for r in &plan.retires {
        sqlx::query("UPDATE asset SET active = false WHERE id = $1")
            .bind(r.id).execute(&mut *tx).await?;
        res.retired += 1;
    }
    for r in &plan.reactivations {
        sqlx::query("UPDATE asset SET active = true WHERE id = $1")
            .bind(r.id).execute(&mut *tx).await?;
        res.reactivated += 1;
    }

    // Removals last, so a purge never pulls the rug from under an edit above.
    // Retire is the default for anything the user did not decide explicitly.
    for r in &plan.removals {
        match modes.get(&r.id).copied().unwrap_or(DeleteMode::Retire) {
            DeleteMode::Retire => {
                sqlx::query("UPDATE asset SET active = false WHERE id = $1")
                    .bind(r.id).execute(&mut *tx).await?;
            }
            DeleteMode::Purge => purge_asset_tx(&mut tx, r.id).await?,
        }
        res.removed += 1;
    }

    tx.commit().await?;
    Ok(res)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration export_then_import apply_import -- --ignored --nocapture
```

Expected: `test result: ok. 3 passed`.

If `export_then_import_is_a_no_op` fails with membership changes, the likely cause is view ordering: `diff` sorts both sides through `sorted()`, so check that `load_db_assets` is not producing duplicate view names from a duplicated `view_asset` row.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/bulk/mod.rs src-tauri/tests/db_integration.rs
git commit -m "feat: export the registry and apply a reviewed import in one transaction"
```

---

## Task 11: Wire the bulk commands into Tauri and the TypeScript client

**Files:**
- Modify: `src-tauri/src/commands.rs` (append a bulk section)
- Modify: `src-tauri/src/lib.rs` (extend `generate_handler!`)
- Modify: `src/lib/api.ts` (types and three client methods)

**Interfaces:**
- Consumes: `bulk::{export_assets_xlsx, preview_import, apply_import, ImportResult}` and `bulk::diff::ImportPlan` from Task 10; `deletion::DeleteMode` from Task 1.
- Produces, callable from the frontend:
  - `export_assets_xlsx(path) -> ()`
  - `preview_assets_import(path) -> ImportPlan`
  - `apply_assets_import(path, fileHash, removalModes, confirmedRemovalCount) -> ImportResult`
  - TypeScript: `ImportPlan`, `ImportResult`, `AddRow`, `EditRow`, `AssetRef`, `MembershipChange`, `InvalidRow`, and `api.exportAssetsXlsx`, `api.previewAssetsImport`, `api.applyAssetsImport`.

- [ ] **Step 1: Append the command section to `src-tauri/src/commands.rs`**

Add `bulk` and `deletion` to the `use crate::{...}` list if not already there, then append:

```rust
// ---------------------------------------------------------------------------
// Bulk assets
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn export_assets_xlsx(state: State<'_, AppState>, path: String)
    -> Result<(), AppError> {
    bulk::export_assets_xlsx(&state.pool, &PathBuf::from(path)).await
}

#[tauri::command]
pub async fn preview_assets_import(state: State<'_, AppState>, path: String)
    -> Result<bulk::diff::ImportPlan, AppError> {
    bulk::preview_import(&state.pool, &PathBuf::from(path)).await
}

#[tauri::command]
pub async fn apply_assets_import(state: State<'_, AppState>, path: String,
                                 file_hash: String,
                                 removal_modes: Vec<(i64, deletion::DeleteMode)>,
                                 confirmed_removal_count: Option<i64>)
    -> Result<bulk::ImportResult, AppError> {
    bulk::apply_import(&state.pool, &PathBuf::from(path), &file_hash,
                       &removal_modes, confirmed_removal_count).await
}
```

- [ ] **Step 2: Register them in `src-tauri/src/lib.rs`**

Inside `generate_handler![...]`, after the deletion commands added in Task 5:

```rust
            commands::export_assets_xlsx, commands::preview_assets_import,
            commands::apply_assets_import,
```

- [ ] **Step 3: Build to verify**

```powershell
cargo build --manifest-path src-tauri/Cargo.toml
```

Expected: compiles clean.

- [ ] **Step 4: Add the TypeScript types and client methods to `src/lib/api.ts`**

Add after the `DeletionImpact` interface:

```typescript
export interface AssetRef { id: number; label: string; security: string; }
export interface AddRow {
  row_number: number; label: string; class: string; id_kind: string;
  ticker: string; isin: string; yellow_key: string; active: boolean;
  security: string; views: string[];
}
export interface EditRow {
  id: number; row_number: number; label: string; class: string; id_kind: string;
  ticker: string; isin: string; yellow_key: string; security: string; changed: string[];
}
export interface MembershipChange {
  id: number; label: string; added: string[]; removed: string[];
}
export interface InvalidRow { row_number: number; reason: string; }
export interface ImportPlan {
  file_hash: string; has_id_column: boolean;
  adds: AddRow[]; edits: EditRow[];
  retires: AssetRef[]; reactivations: AssetRef[];
  membership_changes: MembershipChange[]; removals: AssetRef[];
  invalid_rows: InvalidRow[];
  active_asset_count: number; requires_typed_confirmation: boolean;
}
export interface ImportResult {
  added: number; edited: number; retired: number;
  reactivated: number; membership_updated: number; removed: number;
}
```

and inside the `api` object, after the deletion methods:

```typescript
  exportAssetsXlsx: (path: string) => invoke<void>("export_assets_xlsx", { path }),
  previewAssetsImport: (path: string) => invoke<ImportPlan>("preview_assets_import", { path }),
  applyAssetsImport: (path: string, fileHash: string,
                      removalModes: [number, DeleteMode][],
                      confirmedRemovalCount: number | null) =>
    invoke<ImportResult>("apply_assets_import",
      { path, fileHash, removalModes, confirmedRemovalCount }),
```

- [ ] **Step 5: Type-check the frontend**

```powershell
npm run check
```

Expected: 0 errors.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src/lib/api.ts
git commit -m "feat: expose export, preview and apply to the frontend"
```

---

## Task 12: `ImportDiff.svelte` and the Export/Import controls

**Files:**
- Create: `src/lib/ImportDiff.svelte`
- Modify: `src/lib/AssetsScreen.svelte` (an Excel section)

**Interfaces:**
- Consumes: `api.previewAssetsImport`, `api.applyAssetsImport`, `ImportPlan`, `DeleteMode`, `ImportResult` from Task 11.
- Produces: a Svelte 5 component with props `{ path: string, plan: ImportPlan, onclose: (applied: boolean) => void }`. It owns the per-removal Retire/Purge selectors and the typed confirmation.

- [ ] **Step 1: Create `src/lib/ImportDiff.svelte`**

```svelte
<script lang="ts">
  import { api, type DeleteMode, type ImportPlan } from "./api";

  let { path, plan, onclose }: {
    path: string; plan: ImportPlan; onclose: (applied: boolean) => void;
  } = $props();

  // Retire is the default: the reversible option should never require a click.
  let modes = $state<Record<number, DeleteMode>>(
    Object.fromEntries(plan.removals.map((r) => [r.id, "retire" as DeleteMode])));
  let typed = $state("");
  let error = $state("");
  let busy = $state(false);

  const blocked = $derived(plan.invalid_rows.length > 0);
  const needsCount = $derived(plan.requires_typed_confirmation);
  const countOk = $derived(!needsCount || typed.trim() === String(plan.removals.length));
  const nothingToDo = $derived(
    plan.adds.length === 0 && plan.edits.length === 0 && plan.removals.length === 0 &&
    plan.retires.length === 0 && plan.reactivations.length === 0 &&
    plan.membership_changes.length === 0);

  async function apply() {
    busy = true; error = "";
    try {
      const pairs = plan.removals.map((r) => [r.id, modes[r.id]] as [number, DeleteMode]);
      await api.applyAssetsImport(path, plan.file_hash, pairs,
                                  needsCount ? plan.removals.length : null);
      onclose(true);
    } catch (e) { error = String(e); busy = false; }
  }
</script>

<div class="backdrop">
  <div class="dialog">
    <h3>Import preview</h3>
    {#if error}<p class="error">{error}</p>{/if}

    {#if blocked}
      <p class="error">
        {plan.invalid_rows.length} row(s) must be fixed before anything can be applied.
        Nothing is imported partially.
      </p>
      <ul>
        {#each plan.invalid_rows as r}<li>Row {r.row_number}: {r.reason}</li>{/each}
      </ul>
    {:else if nothingToDo}
      <p>The sheet matches the database. Nothing to do.</p>
    {:else}
      {#if plan.adds.length}
        <h4>Add ({plan.adds.length})</h4>
        <ul>{#each plan.adds as a}<li>{a.label} &mdash; {a.security}</li>{/each}</ul>
      {/if}
      {#if plan.edits.length}
        <h4>Edit ({plan.edits.length})</h4>
        <ul>{#each plan.edits as e}
          <li>{e.label} &mdash; {e.changed.join(", ")} &rarr; {e.security}</li>
        {/each}</ul>
      {/if}
      {#if plan.membership_changes.length}
        <h4>View membership ({plan.membership_changes.length})</h4>
        <ul>{#each plan.membership_changes as m}
          <li>{m.label}
            {#if m.added.length}&nbsp;+{m.added.join(", ")}{/if}
            {#if m.removed.length}&nbsp;&minus;{m.removed.join(", ")}{/if}
          </li>
        {/each}</ul>
      {/if}
      {#if plan.retires.length}
        <h4>Retire ({plan.retires.length})</h4>
        <ul>{#each plan.retires as r}<li>{r.label}</li>{/each}</ul>
      {/if}
      {#if plan.reactivations.length}
        <h4>Reactivate ({plan.reactivations.length})</h4>
        <ul>{#each plan.reactivations as r}<li>{r.label}</li>{/each}</ul>
      {/if}
      {#if plan.removals.length}
        <h4>Missing from the sheet ({plan.removals.length})</h4>
        <table>
          <tbody>
            {#each plan.removals as r}
              <tr>
                <td>{r.label}</td><td class="mono">{r.security}</td>
                <td>
                  <select bind:value={modes[r.id]}>
                    <option value="retire">Retire</option>
                    <option value="purge">Purge</option>
                  </select>
                </td>
              </tr>
            {/each}
          </tbody>
        </table>
      {/if}
      {#if needsCount}
        <p class="warn">
          This removes {plan.removals.length} of {plan.active_asset_count} active assets.
          Type <strong>{plan.removals.length}</strong> to confirm.
        </p>
        <input bind:value={typed} placeholder="removal count" />
      {/if}
    {/if}

    <div class="actions">
      {#if !blocked && !nothingToDo}
        <button onclick={apply} disabled={busy || !countOk}>Apply</button>
      {/if}
      <button onclick={() => onclose(false)} disabled={busy}>Cancel</button>
    </div>
  </div>
</div>

<style>
  .backdrop { position: fixed; inset: 0; background: rgba(0,0,0,0.35);
              display: flex; align-items: center; justify-content: center; }
  .dialog { background: #fff; border-radius: 4px; padding: 1.2rem;
            max-width: 46rem; max-height: 80vh; overflow-y: auto;
            box-shadow: 0 4px 20px rgba(0,0,0,0.3); }
  h3 { margin: 0 0 0.6rem; }
  h4 { margin: 0.9rem 0 0.2rem; }
  ul { margin: 0; padding-left: 1.2rem; color: #444; }
  .error { color: #c00; }
  .warn { color: #a60; }
  .mono { font-family: monospace; }
  table { border-collapse: collapse; margin-top: 0.3rem; }
  td { border: 1px solid #ccc; padding: 0.2rem 0.5rem; }
  .actions { display: flex; gap: 0.5rem; margin-top: 1rem; }
</style>
```

- [ ] **Step 2: Add the Excel section to `src/lib/AssetsScreen.svelte`**

In the `<script>` block, add:

```typescript
  import ImportDiff from "./ImportDiff.svelte";
  import type { ImportPlan } from "./api";

  let sheetPath = $state("");
  let plan = $state<ImportPlan | null>(null);
  let notice = $state("");

  $effect(() => {
    if (!sheetPath) {
      api.getSettings()
        .then((c) => (sheetPath = `${c.data_dir}\\assets.xlsx`))
        .catch(() => (sheetPath = "assets.xlsx"));
    }
  });

  async function exportSheet() {
    notice = ""; error = "";
    try {
      await api.exportAssetsXlsx(sheetPath);
      notice = `Written to ${sheetPath}`;
    } catch (e) { error = String(e); }
  }
  async function previewSheet() {
    notice = ""; error = "";
    try { plan = await api.previewAssetsImport(sheetPath); }
    catch (e) { error = String(e); }
  }
  function afterImport(applied: boolean) {
    plan = null;
    if (applied) { notice = "Import applied."; reload(); }
  }
```

Add this section to the markup, after the assets table and before the closing `</section>`:

```svelte
  <h2>Bulk edit in Excel</h2>
  <p class="note">
    Export writes every asset, its class and identifier, and one column per view.
    Edit it in Excel, then Preview to see exactly what would change before anything
    is applied. Leave <code>id</code> blank on a row to add an asset; delete a row
    to propose removing it. A sheet with no <code>id</code> column can only add and
    edit, never remove.
  </p>
  <div class="bulk">
    <input bind:value={sheetPath} size="48" />
    <button onclick={exportSheet}>Export</button>
    <button onclick={previewSheet}>Preview import</button>
  </div>
  {#if notice}<p class="notice">{notice}</p>{/if}
```

Add to the `<style>` block:

```css
  .bulk { display: flex; gap: 0.5rem; align-items: center; margin-top: 0.5rem; }
  .notice { color: #060; }
```

And add the dialog next to the existing `{#if pending}` block at the end of the markup:

```svelte
{#if plan}
  <ImportDiff path={sheetPath} {plan} onclose={afterImport} />
{/if}
```

- [ ] **Step 3: Type-check and build**

```powershell
npm run check
npm run build
```

Expected: 0 errors.

- [ ] **Step 4: Manual smoke test**

```powershell
cargo build --manifest-path src-tauri/Cargo.toml
.\src-tauri\target\debug\getbloomdata.exe
```

Export, open `C:\bloomdata\assets.xlsx` in Excel, confirm the header is frozen and `class` / `id_kind` / `active` have dropdowns. Add a row with a blank `id`, mark it into a view with an `x`, save, and Preview: expect one add and one membership change and no removals. Apply, and confirm the asset appears in the table. Close the app.

- [ ] **Step 5: Commit**

```bash
git add src/lib/ImportDiff.svelte src/lib/AssetsScreen.svelte
git commit -m "feat: Excel export and reviewed import in the assets screen"
```

---

## Task 13: Full-suite verification

**Files:**
- Modify: none expected. Fix whatever the suite reports.

**Interfaces:**
- Consumes: everything from Tasks 1-12.
- Produces: a green suite and a squashed record of it.

- [ ] **Step 1: Close the app**

The running executable holds a lock that makes `cargo build` and `cargo test` fail with `os error 5 / Accès refusé`.

```powershell
Get-Process getbloomdata -ErrorAction SilentlyContinue | Stop-Process
```

- [ ] **Step 2: Run the Rust unit tests**

```powershell
cargo test --manifest-path src-tauri/Cargo.toml
```

Expected: all green. The baseline before this plan was 39 unit tests; this plan adds 17 in `bulk::sheet` and `bulk::diff`.

- [ ] **Step 3: Run the integration tests**

```powershell
$env:BLOOM_TEST_DATABASE_URL = [Environment]::GetEnvironmentVariable("BLOOM_TEST_DATABASE_URL", "User")
cargo test --manifest-path src-tauri/Cargo.toml --test db_integration -- --ignored
```

Expected: all green. The baseline was 8 (plus one live-Bloomberg smoke test that only runs on the Bloomberg machine); this plan adds 11.

- [ ] **Step 4: Run the Python sidecar tests**

```powershell
python src-tauri/scripts/test_blp_fetch.py
```

Expected: 21 tests, OK. (`pytest` is not installed on this machine; the module is unittest-based and runs directly.)

- [ ] **Step 5: Type-check and build the frontend**

```powershell
npm run check
npm run build
```

Expected: 0 errors across 139+ files.

- [ ] **Step 6: Confirm the design boundary held**

```bash
grep -nE "use (sqlx|std::fs|calamine|rust_xlsxwriter)" src-tauri/src/bulk/diff.rs
grep -rn "ON DELETE CASCADE" src-tauri/migrations/
git status --short src-tauri/migrations/
```

Expected: no output from the first command; the second matches only the two pre-existing `view_asset` / `view_field` lines in `0001_init.sql`; the third is empty, proving no migration was added.

- [ ] **Step 7: Commit any fixes and push the branch**

```bash
git add -A
git commit -m "test: full suite green for deletion and bulk asset management"
git push -u origin deletion-and-bulk-assets
```

---

## Self-review

**Spec coverage.** Every numbered section maps to a task:

| Spec | Task |
|---|---|
| §3.1 per-deletion choice | 6 (`DeleteDialog` offers only the modes the impact reports) |
| §3.2 rules fit the entity | 2, 3, 4 (schedule and class take no `DeleteMode`) |
| §3.3 explicit purge, restrictive FKs | 3, 4, and the Task 13 grep |
| §3.4 `run`/`hit_ledger` untouched | 3 (asserted in `purging_an_asset_...`) |
| §3.5 sheet = registry + one column per view | 7, 8 |
| §3.6 missing row = proposed removal | 9, 12 |
| §3.7 `.xlsx` via `rust_xlsxwriter`/`calamine` | 7, 8 |
| §4 no migration; scheduler `view.active` | 4 (`due_schedules`), 13 (grep) |
| §5 per-entity semantics and purge order | 2, 3, 4 |
| §6 command surface | 5, 11 |
| §7 sheet contract | 7 (writer), 8 (reader) |
| §8 two-phase import | 10 |
| §8.1 three guardrails | 9 (1 and 2), 10 (2 server-side), 12 (3) |
| §9 invalid rows block everything | 9 (collection), 10 (rejection) |
| §10 UI changes | 6, 12 |
| §11 testing | 3, 4, 9, 10, 13 |

**Placeholders.** None. Every code step carries the code; every test step carries the assertions. The two places that say "read the file first" (Task 6 Step 3, placing buttons in `ViewsScreen.svelte`) give the exact markup to insert and only leave its position to the implementer, because the surrounding markup is 174 lines this plan should not reproduce.

**Type consistency.** `DeleteMode` and `EntityKind` are defined once in Task 1 and used verbatim in 2, 3, 4, 5, 6, 10, 11, 12. `ExportRow` is defined in Task 7 and consumed in 8 and 10. `SheetData`/`SheetRow` are defined in Task 8 and consumed in 9 and 10. `DbAsset`/`ImportPlan` are defined in Task 9 and consumed in 10, 11, 12. `AssetRef` covers `retires`, `reactivations` and `removals` in all three places. The serde renaming (`snake_case`) matches the TypeScript string unions in Tasks 5 and 11.

**One gap found and closed during review:** the spec's `apply_assets_import` signature had nowhere to carry guardrail 2's typed count. Resolved in the "Deviations" section and implemented in Tasks 10-12 as `confirmed_removal_count: Option<i64>`.
