# Bloomberg EOD Data Pipeline Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A Tauri 2 desktop app that generates a single Bloomberg-formula Excel workbook per run, refreshes it unattended via a PowerShell COM driver, and ingests the values idempotently into PostgreSQL + TimescaleDB.

**Architecture:** Rust core behind a Tauri 2 shell with a Svelte 5 frontend. The 4-stage pipeline (generate → refresh → read → ingest) lives in focused Rust modules behind a `DataFetcher` trait; the only COM code is a standalone-debuggable `refresh.ps1` spawned as a child process. Long-format `observation` hypertable keyed `(asset_id, field_id, obs_date)` with upserts makes every run idempotent.

**Tech Stack:** Rust (edition 2021), Tauri 2, Svelte 5 + TypeScript, sqlx 0.8 (postgres, runtime-tokio, chrono), tokio 1, rust_xlsxwriter, calamine, rand, PostgreSQL 16 + TimescaleDB, PowerShell 5.1+ (COM).

**Spec:** `docs/superpowers/specs/2026-08-13-bloomberg-eod-pipeline-design.md` — read it before starting any task.

## Global Constraints

- One run produces **exactly one** Excel workbook for the whole view. Never one file per asset.
- `observation` PK is `(asset_id, field_id, obs_date)`; all ingest is `INSERT ... ON CONFLICT ... DO UPDATE` (idempotent).
- Adding a data field is an `INSERT` into `field_def` — never a schema migration.
- Defaults, exact values from spec: schedule window **09:00–18:00 local**; soft hit threshold **100,000**/day; hard-confirm threshold **soft × 2**; backfill cap **30 days** per run.
- Hit estimator: BDP ≈ 1 hit per security × field; BDH ≈ 1 hit per security × field × returned day.
- `bdp_security` resolution: `id_kind='ticker'` → `"{ticker} {yellow_key}"`; `id_kind='isin'` → `"/isin/{isin} {yellow_key}"`.
- Workbooks live in `pending/` during a run and move to `archive/YYYY/MM/run_<run_id>_<view>_<date>.xlsx` after successful ingest.
- PowerShell driver exit codes: `0` ok, `2` timeout, `3` excel/COM error; one-line JSON status on stdout.
- DB-backed tests read env var `BLOOM_TEST_DATABASE_URL` and are marked `#[ignore = "requires postgres"]`; run them with `cargo test -- --ignored` when a local TimescaleDB is up. All other tests must pass without any database or Excel.
- Commit style: conventional commits (`feat:`, `test:`, `chore:`); commit at the end of every task at minimum.
- Windows paths: the Rust code must build paths with `PathBuf`, never hard-coded separators.

## File Structure

```
getbloomdata/
├── package.json, svelte.config.js, vite.config.ts   # Svelte 5 frontend (Tauri template)
├── src/                        # frontend
│   ├── App.svelte              # tab shell: Assets | Views | Run | Settings
│   ├── lib/api.ts              # typed invoke() wrappers, one per Tauri command
│   ├── lib/AssetsScreen.svelte
│   ├── lib/ViewsScreen.svelte
│   ├── lib/RunScreen.svelte
│   └── lib/SettingsScreen.svelte
└── src-tauri/
    ├── Cargo.toml
    ├── tauri.conf.json
    ├── migrations/0001_init.sql        # full schema (sqlx migrate)
    ├── scripts/refresh.ps1             # the ONLY COM code in the project
    ├── src/
    │   ├── main.rs                     # tauri entry
    │   ├── lib.rs                      # module tree + AppState
    │   ├── error.rs                    # AppError (thiserror)
    │   ├── db.rs                       # pool init + migrations
    │   ├── registry.rs                 # asset classes + assets, bdp_security
    │   ├── fields.rs                   # field_def catalog
    │   ├── views.rs                    # view / view_asset / view_field
    │   ├── excel_gen.rs                # workbook generation (BDP + BDH + META)
    │   ├── refresh_driver.rs           # spawn refresh.ps1, parse exit/JSON
    │   ├── excel_read.rs               # calamine read + cell classification
    │   ├── ingest.rs                   # validation + upserts + ingest_issue
    │   ├── budget.rs                   # hit estimator + ledger + thresholds
    │   ├── orchestrator.rs             # 4-stage pipeline, run rows, archive
    │   ├── scheduler.rs                # random draw, catch-up, gap detection
    │   └── commands.rs                 # #[tauri::command] IPC layer
    └── tests/
        └── db_integration.rs           # #[ignore]d tests vs live TimescaleDB
```

Module dependency direction (no cycles): `commands` → `orchestrator`/`scheduler` → `excel_gen`/`refresh_driver`/`excel_read`/`ingest`/`budget` → `registry`/`fields`/`views` → `db`/`error`. Pure logic (bdp_security, estimator, gap math, cell classification, layout) is written as free functions on plain structs so it unit-tests without DB, Excel, or Tauri.

---

### Task 1: Project scaffold

**Files:**
- Create: entire Tauri + Svelte template (generated), `src-tauri/Cargo.toml` (edited), `.gitignore` (edited)

**Interfaces:**
- Produces: a building Tauri 2 + Svelte 5 + TS app; `src-tauri` crate named `getbloomdata` with all runtime deps declared. Later tasks add modules to `src-tauri/src/`.

- [ ] **Step 1: Generate the template into the repo root**

Run from `getbloomdata/` (dir already contains `docs/` and `.git` — the scaffolder must target the current dir):

```powershell
npm create tauri-app@latest temp-scaffold -- --manager npm --template svelte-ts --yes
# move generated content up, keeping existing docs/ and .git/
Get-ChildItem temp-scaffold -Force | Move-Item -Destination . -Force
Remove-Item temp-scaffold -Recurse -Force
npm install
```

- [ ] **Step 2: Declare Rust dependencies**

Edit `src-tauri/Cargo.toml` `[dependencies]` to exactly:

```toml
[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-opener = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
sqlx = { version = "0.8", features = ["runtime-tokio", "postgres", "chrono", "migrate"] }
tokio = { version = "1", features = ["full"] }
chrono = { version = "0.4", features = ["serde"] }
rust_xlsxwriter = "0.79"
calamine = "0.26"
rand = "0.8"
thiserror = "1"

[dev-dependencies]
tempfile = "3"
```

(If the template pinned different versions of tauri/serde, keep the template's tauri version line; the rest as written.)

- [ ] **Step 3: Verify it builds and runs**

Run: `cargo build --manifest-path src-tauri/Cargo.toml`
Expected: compiles with no errors (warnings ok).

Run: `npm run tauri dev` briefly — expected: the template window opens. Close it.

- [ ] **Step 4: Ensure .gitignore covers artifacts**

Append to `.gitignore` if missing: `node_modules/`, `src-tauri/target/`, `dist/`, `pending/`, `archive/`.

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore: scaffold tauri 2 + svelte 5 app with core rust dependencies"
```

---

### Task 2: Database schema and pool (`db.rs`, migration)

**Files:**
- Create: `src-tauri/migrations/0001_init.sql`
- Create: `src-tauri/src/db.rs`, `src-tauri/src/error.rs`
- Modify: `src-tauri/src/lib.rs` (declare modules)
- Test: `src-tauri/tests/db_integration.rs`

**Interfaces:**
- Produces: `db::connect(database_url: &str) -> Result<sqlx::PgPool, AppError>` (runs migrations on connect); `error::AppError` enum with `#[from] sqlx::Error`, `Io(#[from] std::io::Error)`, `Excel(String)`, `Refresh { code: i32, detail: String }`, `Validation(String)`; `type AppResult<T> = Result<T, AppError>`. All later tasks return `AppResult`.

- [ ] **Step 1: Write the migration**

`src-tauri/migrations/0001_init.sql` — the complete schema from spec §3 (verbatim; `trigger_kind` because `trigger` is awkward in SQL):

```sql
CREATE EXTENSION IF NOT EXISTS timescaledb;

CREATE TABLE asset_class (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE asset (
  id             BIGSERIAL PRIMARY KEY,
  asset_class_id BIGINT NOT NULL REFERENCES asset_class(id),
  label          TEXT NOT NULL,
  id_kind        TEXT NOT NULL CHECK (id_kind IN ('ticker','isin')),
  ticker         TEXT,
  isin           TEXT,
  yellow_key     TEXT NOT NULL,
  bdp_security   TEXT NOT NULL,
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  CHECK ((id_kind = 'ticker' AND ticker IS NOT NULL)
      OR (id_kind = 'isin'   AND isin   IS NOT NULL)),
  UNIQUE (bdp_security)
);

CREATE TABLE field_def (
  id             BIGSERIAL PRIMARY KEY,
  asset_class_id BIGINT NOT NULL REFERENCES asset_class(id),
  mnemonic       TEXT NOT NULL,
  label          TEXT NOT NULL,
  value_kind     TEXT NOT NULL CHECK (value_kind IN ('numeric','text','date')),
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  UNIQUE (asset_class_id, mnemonic)
);

CREATE TABLE view (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT '',
  active      BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE view_asset (
  view_id  BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  asset_id BIGINT NOT NULL REFERENCES asset(id),
  PRIMARY KEY (view_id, asset_id)
);

CREATE TABLE view_field (
  view_id  BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  field_id BIGINT NOT NULL REFERENCES field_def(id),
  PRIMARY KEY (view_id, field_id)
);

CREATE TABLE run (
  id             BIGSERIAL PRIMARY KEY,
  view_id        BIGINT NOT NULL REFERENCES view(id),
  kind           TEXT NOT NULL CHECK (kind IN ('eod','backfill')),
  trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('manual','scheduled')),
  status         TEXT NOT NULL CHECK (status IN
    ('generating','refreshing','reading','ingesting','ok','failed','partial')),
  started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at    TIMESTAMPTZ,
  workbook_path  TEXT,
  estimated_hits BIGINT NOT NULL DEFAULT 0,
  error_summary  TEXT
);

CREATE TABLE observation (
  asset_id    BIGINT NOT NULL REFERENCES asset(id),
  field_id    BIGINT NOT NULL REFERENCES field_def(id),
  obs_date    DATE   NOT NULL,
  value_num   DOUBLE PRECISION,
  value_text  TEXT,
  run_id      BIGINT NOT NULL REFERENCES run(id),
  ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (asset_id, field_id, obs_date),
  CHECK ((value_num IS NULL) <> (value_text IS NULL))
);
SELECT create_hypertable('observation', 'obs_date');

CREATE TABLE ingest_issue (
  id       BIGSERIAL PRIMARY KEY,
  run_id   BIGINT NOT NULL REFERENCES run(id),
  asset_id BIGINT REFERENCES asset(id),
  field_id BIGINT REFERENCES field_def(id),
  obs_date DATE,
  severity TEXT NOT NULL CHECK (severity IN ('warn','error')),
  code     TEXT NOT NULL,
  detail   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE hit_ledger (
  id             BIGSERIAL PRIMARY KEY,
  run_id         BIGINT NOT NULL REFERENCES run(id),
  estimated_hits BIGINT NOT NULL,
  occurred_on    DATE NOT NULL DEFAULT CURRENT_DATE
);

CREATE TABLE schedule (
  id           BIGSERIAL PRIMARY KEY,
  view_id      BIGINT NOT NULL REFERENCES view(id),
  active       BOOLEAN NOT NULL DEFAULT TRUE,
  window_start TIME NOT NULL DEFAULT '09:00',
  window_end   TIME NOT NULL DEFAULT '18:00',
  drawn_for    DATE,
  drawn_at     TIME,
  last_result  TEXT
);
```

- [ ] **Step 2: Write `error.rs`**

```rust
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("database error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("migration error: {0}")]
    Migrate(#[from] sqlx::migrate::MigrateError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("excel error: {0}")]
    Excel(String),
    #[error("refresh driver failed (exit {code}): {detail}")]
    Refresh { code: i32, detail: String },
    #[error("validation error: {0}")]
    Validation(String),
}

pub type AppResult<T> = Result<T, AppError>;

// Tauri commands need serializable errors.
impl serde::Serialize for AppError {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
```

- [ ] **Step 3: Write `db.rs`**

```rust
use crate::error::AppResult;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

pub async fn connect(database_url: &str) -> AppResult<PgPool> {
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(database_url)
        .await?;
    sqlx::migrate!("./migrations").run(&pool).await?;
    Ok(pool)
}
```

Declare in `lib.rs`: `pub mod db; pub mod error;`

- [ ] **Step 4: Write the failing integration test**

`src-tauri/tests/db_integration.rs`:

```rust
use sqlx::Row;

fn test_url() -> Option<String> {
    std::env::var("BLOOM_TEST_DATABASE_URL").ok()
}

#[tokio::test]
#[ignore = "requires postgres"]
async fn migration_creates_all_tables() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let rows = sqlx::query(
        "SELECT table_name FROM information_schema.tables
         WHERE table_schema = 'public'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    let names: Vec<String> = rows.iter().map(|r| r.get("table_name")).collect();
    for t in ["asset_class","asset","field_def","view","view_asset","view_field",
              "run","observation","ingest_issue","hit_ledger","schedule"] {
        assert!(names.iter().any(|n| n == t), "missing table {t}");
    }
}
```

(The template names the lib crate `getbloomdata_lib` or similar — use whatever `[lib] name` the template generated; keep it consistent in every later test.)

- [ ] **Step 5: Compile-check, then run against live DB if available**

Run: `cargo test --manifest-path src-tauri/Cargo.toml` — expected: PASS (ignored test skipped, everything compiles).
If a local TimescaleDB is up: create a scratch DB, set `BLOOM_TEST_DATABASE_URL=postgres://postgres:<pw>@localhost/bloom_test`, run `cargo test -- --ignored` — expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/migrations src-tauri/src src-tauri/tests
git commit -m "feat: postgres schema, migrations, pool, and error type"
```

---

### Task 3: Registry — asset classes, assets, `bdp_security` resolution (`registry.rs`)

**Files:**
- Create: `src-tauri/src/registry.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod registry;`)
- Test: unit tests in-module; DB tests appended to `src-tauri/tests/db_integration.rs`

**Interfaces:**
- Consumes: `db::connect`, `AppResult` (Task 2).
- Produces:
  - `pub struct AssetClass { pub id: i64, pub name: String, pub description: String }`
  - `pub struct Asset { pub id: i64, pub asset_class_id: i64, pub label: String, pub id_kind: String, pub ticker: Option<String>, pub isin: Option<String>, pub yellow_key: String, pub bdp_security: String, pub active: bool }` (derives `serde::Serialize, serde::Deserialize, sqlx::FromRow, Clone, Debug` — same derives on every DB struct in this plan)
  - `pub struct NewAsset { pub asset_class_id: i64, pub label: String, pub id_kind: String, pub ticker: Option<String>, pub isin: Option<String>, pub yellow_key: String }`
  - `pub fn resolve_bdp_security(id_kind: &str, ticker: Option<&str>, isin: Option<&str>, yellow_key: &str) -> AppResult<String>`
  - `pub async fn create_asset_class(pool: &PgPool, name: &str, description: &str) -> AppResult<AssetClass>`
  - `pub async fn list_asset_classes(pool: &PgPool) -> AppResult<Vec<AssetClass>>`
  - `pub async fn create_asset(pool: &PgPool, new: NewAsset) -> AppResult<Asset>`
  - `pub async fn list_assets(pool: &PgPool) -> AppResult<Vec<Asset>>`
  - `pub async fn set_asset_active(pool: &PgPool, asset_id: i64, active: bool) -> AppResult<()>`

- [ ] **Step 1: Write failing unit tests for `resolve_bdp_security`**

In `registry.rs` bottom:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_kind_joins_ticker_and_yellow_key() {
        let s = resolve_bdp_security("ticker", Some("AAPL US"), None, "Equity").unwrap();
        assert_eq!(s, "AAPL US Equity");
    }

    #[test]
    fn isin_kind_builds_slash_isin_form() {
        let s = resolve_bdp_security("isin", None, Some("FR0000120271"), "Corp").unwrap();
        assert_eq!(s, "/isin/FR0000120271 Corp");
    }

    #[test]
    fn missing_identifier_is_validation_error() {
        assert!(resolve_bdp_security("ticker", None, None, "Equity").is_err());
        assert!(resolve_bdp_security("isin", None, None, "Corp").is_err());
        assert!(resolve_bdp_security("cusip", Some("X"), None, "Corp").is_err());
    }

    #[test]
    fn inputs_are_trimmed() {
        let s = resolve_bdp_security("ticker", Some(" AAPL US "), None, " Equity ").unwrap();
        assert_eq!(s, "AAPL US Equity");
    }
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test --manifest-path src-tauri/Cargo.toml registry` — expected: FAIL to compile (`resolve_bdp_security` not defined).

- [ ] **Step 3: Implement `registry.rs`**

```rust
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct AssetClass {
    pub id: i64,
    pub name: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Asset {
    pub id: i64,
    pub asset_class_id: i64,
    pub label: String,
    pub id_kind: String,
    pub ticker: Option<String>,
    pub isin: Option<String>,
    pub yellow_key: String,
    pub bdp_security: String,
    pub active: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NewAsset {
    pub asset_class_id: i64,
    pub label: String,
    pub id_kind: String,
    pub ticker: Option<String>,
    pub isin: Option<String>,
    pub yellow_key: String,
}

pub fn resolve_bdp_security(
    id_kind: &str,
    ticker: Option<&str>,
    isin: Option<&str>,
    yellow_key: &str,
) -> AppResult<String> {
    let yk = yellow_key.trim();
    if yk.is_empty() {
        return Err(AppError::Validation("yellow_key is required".into()));
    }
    match id_kind {
        "ticker" => {
            let t = ticker.map(str::trim).filter(|t| !t.is_empty())
                .ok_or_else(|| AppError::Validation("ticker required for id_kind=ticker".into()))?;
            Ok(format!("{t} {yk}"))
        }
        "isin" => {
            let i = isin.map(str::trim).filter(|i| !i.is_empty())
                .ok_or_else(|| AppError::Validation("isin required for id_kind=isin".into()))?;
            Ok(format!("/isin/{i} {yk}"))
        }
        other => Err(AppError::Validation(format!("unknown id_kind '{other}'"))),
    }
}

pub async fn create_asset_class(pool: &PgPool, name: &str, description: &str) -> AppResult<AssetClass> {
    Ok(sqlx::query_as::<_, AssetClass>(
        "INSERT INTO asset_class (name, description) VALUES ($1, $2) RETURNING *")
        .bind(name).bind(description).fetch_one(pool).await?)
}

pub async fn list_asset_classes(pool: &PgPool) -> AppResult<Vec<AssetClass>> {
    Ok(sqlx::query_as::<_, AssetClass>("SELECT * FROM asset_class ORDER BY name")
        .fetch_all(pool).await?)
}

pub async fn create_asset(pool: &PgPool, new: NewAsset) -> AppResult<Asset> {
    let sec = resolve_bdp_security(&new.id_kind, new.ticker.as_deref(),
                                   new.isin.as_deref(), &new.yellow_key)?;
    Ok(sqlx::query_as::<_, Asset>(
        "INSERT INTO asset (asset_class_id, label, id_kind, ticker, isin, yellow_key, bdp_security)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING *")
        .bind(new.asset_class_id).bind(&new.label).bind(&new.id_kind)
        .bind(&new.ticker).bind(&new.isin).bind(new.yellow_key.trim()).bind(sec)
        .fetch_one(pool).await?)
}

pub async fn list_assets(pool: &PgPool) -> AppResult<Vec<Asset>> {
    Ok(sqlx::query_as::<_, Asset>("SELECT * FROM asset ORDER BY label")
        .fetch_all(pool).await?)
}

pub async fn set_asset_active(pool: &PgPool, asset_id: i64, active: bool) -> AppResult<()> {
    sqlx::query("UPDATE asset SET active = $2 WHERE id = $1")
        .bind(asset_id).bind(active).execute(pool).await?;
    Ok(())
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test --manifest-path src-tauri/Cargo.toml registry` — expected: 4 PASS.

- [ ] **Step 5: Add DB round-trip test (ignored) to `tests/db_integration.rs`**

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn asset_crud_round_trip() {
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = getbloomdata_lib::db::connect(&url).await.unwrap();
    let class = getbloomdata_lib::registry::create_asset_class(&pool, "EquityT3", "test").await.unwrap();
    let asset = getbloomdata_lib::registry::create_asset(&pool, getbloomdata_lib::registry::NewAsset {
        asset_class_id: class.id,
        label: "Apple".into(),
        id_kind: "ticker".into(),
        ticker: Some("AAPL US".into()),
        isin: None,
        yellow_key: "Equity".into(),
    }).await.unwrap();
    assert_eq!(asset.bdp_security, "AAPL US Equity");
}
```

Run (with DB up): `cargo test -- --ignored` — expected: PASS. Without DB: `cargo test` still fully green.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src src-tauri/tests
git commit -m "feat: asset registry with bdp_security resolution"
```

---

### Task 4: Field catalog and views (`fields.rs`, `views.rs`)

**Files:**
- Create: `src-tauri/src/fields.rs`, `src-tauri/src/views.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod fields; pub mod views;`)
- Test: unit tests in-module; DB tests appended to `src-tauri/tests/db_integration.rs`

**Interfaces:**
- Consumes: Task 2/3 types.
- Produces:
  - `pub struct FieldDef { pub id: i64, pub asset_class_id: i64, pub mnemonic: String, pub label: String, pub value_kind: String, pub active: bool }`
  - `fields::normalize_mnemonic(m: &str) -> String`; `fields::validate_value_kind(k: &str) -> AppResult<()>`
  - `fields::create_field(pool, asset_class_id: i64, mnemonic: &str, label: &str, value_kind: &str) -> AppResult<FieldDef>` — rejects `value_kind` not in `numeric|text|date`; uppercases + trims mnemonic.
  - `fields::list_fields(pool) -> AppResult<Vec<FieldDef>>`
  - `pub struct View { pub id: i64, pub name: String, pub description: String, pub active: bool }`
  - `views::create_view(pool, name: &str, description: &str) -> AppResult<View>`
  - `views::list_views(pool) -> AppResult<Vec<View>>`
  - `views::set_view_assets(pool, view_id: i64, asset_ids: &[i64]) -> AppResult<()>` (delete-then-insert in one transaction)
  - `views::set_view_fields(pool, view_id: i64, field_ids: &[i64]) -> AppResult<()>` (same pattern)
  - `views::view_assets(pool, view_id: i64) -> AppResult<Vec<Asset>>` (active assets only)
  - `views::view_fields(pool, view_id: i64) -> AppResult<Vec<FieldDef>>` — **if `view_field` is empty for this view, returns all active fields of the classes present in the view's assets** (the spec's default).

- [ ] **Step 1: Write failing unit tests in `fields.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_mnemonic_uppercases_and_trims() {
        assert_eq!(normalize_mnemonic(" px_last "), "PX_LAST");
    }

    #[test]
    fn invalid_value_kind_rejected() {
        assert!(validate_value_kind("numeric").is_ok());
        assert!(validate_value_kind("text").is_ok());
        assert!(validate_value_kind("date").is_ok());
        assert!(validate_value_kind("blob").is_err());
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --manifest-path src-tauri/Cargo.toml fields` → compile FAIL.

- [ ] **Step 3: Implement `fields.rs`**

```rust
use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct FieldDef {
    pub id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
    pub label: String,
    pub value_kind: String,
    pub active: bool,
}

pub fn normalize_mnemonic(m: &str) -> String {
    m.trim().to_uppercase()
}

pub fn validate_value_kind(k: &str) -> AppResult<()> {
    match k {
        "numeric" | "text" | "date" => Ok(()),
        other => Err(AppError::Validation(format!("invalid value_kind '{other}'"))),
    }
}

pub async fn create_field(pool: &PgPool, asset_class_id: i64, mnemonic: &str,
                          label: &str, value_kind: &str) -> AppResult<FieldDef> {
    validate_value_kind(value_kind)?;
    Ok(sqlx::query_as::<_, FieldDef>(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,$2,$3,$4) RETURNING *")
        .bind(asset_class_id).bind(normalize_mnemonic(mnemonic))
        .bind(label).bind(value_kind).fetch_one(pool).await?)
}

pub async fn list_fields(pool: &PgPool) -> AppResult<Vec<FieldDef>> {
    Ok(sqlx::query_as::<_, FieldDef>(
        "SELECT * FROM field_def ORDER BY asset_class_id, mnemonic")
        .fetch_all(pool).await?)
}
```

- [ ] **Step 4: Implement `views.rs`**

```rust
use crate::error::AppResult;
use crate::fields::FieldDef;
use crate::registry::Asset;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct View {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub active: bool,
}

pub async fn create_view(pool: &PgPool, name: &str, description: &str) -> AppResult<View> {
    Ok(sqlx::query_as::<_, View>(
        "INSERT INTO view (name, description) VALUES ($1,$2) RETURNING *")
        .bind(name).bind(description).fetch_one(pool).await?)
}

pub async fn list_views(pool: &PgPool) -> AppResult<Vec<View>> {
    Ok(sqlx::query_as::<_, View>("SELECT * FROM view ORDER BY name")
        .fetch_all(pool).await?)
}

pub async fn set_view_assets(pool: &PgPool, view_id: i64, asset_ids: &[i64]) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_asset WHERE view_id = $1")
        .bind(view_id).execute(&mut *tx).await?;
    for aid in asset_ids {
        sqlx::query("INSERT INTO view_asset (view_id, asset_id) VALUES ($1,$2)")
            .bind(view_id).bind(aid).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn set_view_fields(pool: &PgPool, view_id: i64, field_ids: &[i64]) -> AppResult<()> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_field WHERE view_id = $1")
        .bind(view_id).execute(&mut *tx).await?;
    for fid in field_ids {
        sqlx::query("INSERT INTO view_field (view_id, field_id) VALUES ($1,$2)")
            .bind(view_id).bind(fid).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn view_assets(pool: &PgPool, view_id: i64) -> AppResult<Vec<Asset>> {
    Ok(sqlx::query_as::<_, Asset>(
        "SELECT a.* FROM asset a
         JOIN view_asset va ON va.asset_id = a.id
         WHERE va.view_id = $1 AND a.active ORDER BY a.label")
        .bind(view_id).fetch_all(pool).await?)
}

pub async fn view_fields(pool: &PgPool, view_id: i64) -> AppResult<Vec<FieldDef>> {
    let explicit = sqlx::query_as::<_, FieldDef>(
        "SELECT f.* FROM field_def f
         JOIN view_field vf ON vf.field_id = f.id
         WHERE vf.view_id = $1 AND f.active ORDER BY f.asset_class_id, f.mnemonic")
        .bind(view_id).fetch_all(pool).await?;
    if !explicit.is_empty() {
        return Ok(explicit);
    }
    // Spec default: all active fields of the classes present in the view's assets.
    Ok(sqlx::query_as::<_, FieldDef>(
        "SELECT DISTINCT f.* FROM field_def f
         JOIN asset a ON a.asset_class_id = f.asset_class_id
         JOIN view_asset va ON va.asset_id = a.id
         WHERE va.view_id = $1 AND f.active AND a.active
         ORDER BY f.asset_class_id, f.mnemonic")
        .bind(view_id).fetch_all(pool).await?)
}
```

- [ ] **Step 5: Run unit tests** — `cargo test --manifest-path src-tauri/Cargo.toml` → all PASS.

- [ ] **Step 6: Add ignored DB test — default-field fallback**

Append to `tests/db_integration.rs`:

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn view_fields_falls_back_to_class_fields() {
    use getbloomdata_lib::{db, fields, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, "EquityT4", "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST", "Last price", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "MC".into(), id_kind: "isin".into(),
        ticker: None, isin: Some("FR0000121014".into()), yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, "lux-t4", "").await.unwrap();
    views::set_view_assets(&pool, v.id, &[a.id]).await.unwrap();
    let fs = views::view_fields(&pool, v.id).await.unwrap();  // no explicit fields
    assert!(fs.iter().any(|x| x.id == f.id));
}
```

Run with DB: `cargo test -- --ignored` → PASS.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src src-tauri/tests
git commit -m "feat: field catalog and views with default field-set fallback"
```

---

### Task 5: EOD workbook generation (`excel_gen.rs`)

**Files:**
- Create: `src-tauri/src/excel_gen.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod excel_gen;`)
- Test: unit tests in-module (tempfile + calamine re-read; no Excel, no DB)

**Interfaces:**
- Consumes: `AppResult` (Task 2). Plain input structs only — this module never touches the DB.
- Produces (later tasks depend on these exact names):
  - `pub const LAYOUT_VERSION: i64 = 1;`
  - `pub struct WbMeta { pub run_id: i64, pub view_id: i64, pub kind: String, pub generated_at: String }` (timestamp passed in by the orchestrator, never computed here)
  - `pub struct GenAsset { pub asset_id: i64, pub asset_class_id: i64, pub class_name: String, pub label: String, pub bdp_security: String }`
  - `pub struct GenField { pub field_id: i64, pub asset_class_id: i64, pub mnemonic: String }`
  - `pub fn sanitize_sheet_name(raw: &str) -> String` — strips `[ ] : * ? / \`, truncates to 31 chars, never empty (falls back to `"Sheet"`).
  - `pub fn generate_eod_workbook(path: &Path, meta: &WbMeta, assets: &[GenAsset], fields: &[GenField]) -> AppResult<()>`

**Workbook layout (spec §4 stage 1, EOD):** one visible sheet per asset class present in `assets`, named `sanitize_sheet_name(class_name)`. Row 1: `A1 = "SECURITY"`, then the class's field mnemonics left-to-right from B1. Column A from row 2: each asset's `bdp_security` as plain text. Each data cell: `=BDP($A<row>,"<MNEMONIC>")`. Hidden sheet `META`: rows `run_id`, `view_id`, `kind`, `generated_at`, `layout_version` as key in column A / value in column B.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{open_workbook, Reader, Xlsx};

    fn sample() -> (Vec<GenAsset>, Vec<GenField>) {
        let assets = vec![
            GenAsset { asset_id: 1, asset_class_id: 10, class_name: "Equity".into(),
                       label: "Apple".into(), bdp_security: "AAPL US Equity".into() },
            GenAsset { asset_id: 2, asset_class_id: 10, class_name: "Equity".into(),
                       label: "LVMH".into(), bdp_security: "/isin/FR0000121014 Equity".into() },
            GenAsset { asset_id: 3, asset_class_id: 20, class_name: "Index".into(),
                       label: "EuroStoxx".into(), bdp_security: "SX5E Index".into() },
        ];
        let fields = vec![
            GenField { field_id: 100, asset_class_id: 10, mnemonic: "PX_LAST".into() },
            GenField { field_id: 101, asset_class_id: 10, mnemonic: "PX_VOLUME".into() },
            GenField { field_id: 200, asset_class_id: 20, mnemonic: "PX_LAST".into() },
        ];
        (assets, fields)
    }

    #[test]
    fn sheet_name_sanitized() {
        assert_eq!(sanitize_sheet_name("FX/Rates: EUR*"), "FXRates EUR");
        assert_eq!(sanitize_sheet_name(""), "Sheet");
        assert_eq!(sanitize_sheet_name(&"x".repeat(40)).len(), 31);
    }

    #[test]
    fn eod_workbook_has_one_sheet_per_class_plus_meta() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wb.xlsx");
        let (assets, fields) = sample();
        let meta = WbMeta { run_id: 7, view_id: 3, kind: "eod".into(),
                            generated_at: "2026-08-13T10:00:00".into() };
        generate_eod_workbook(&path, &meta, &assets, &fields).unwrap();

        let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
        let names = wb.sheet_names().to_vec();
        assert!(names.contains(&"Equity".to_string()));
        assert!(names.contains(&"Index".to_string()));
        assert!(names.contains(&"META".to_string()));

        // header row + securities in column A
        let r = wb.worksheet_range("Equity").unwrap();
        assert_eq!(r.get_value((0, 0)).unwrap().to_string(), "SECURITY");
        assert_eq!(r.get_value((0, 1)).unwrap().to_string(), "PX_LAST");
        assert_eq!(r.get_value((1, 0)).unwrap().to_string(), "AAPL US Equity");

        // BDP formulas present
        let f = wb.worksheet_formula("Equity").unwrap();
        let cell = f.get_value((1, 1)).unwrap().to_string();
        assert!(cell.contains("BDP($A2,\"PX_LAST\")"), "got formula: {cell}");

        // META carries run identity
        let m = wb.worksheet_range("META").unwrap();
        assert_eq!(m.get_value((0, 0)).unwrap().to_string(), "run_id");
        assert_eq!(m.get_value((0, 1)).unwrap().to_string(), "7");
        assert_eq!(m.get_value((4, 0)).unwrap().to_string(), "layout_version");
        assert_eq!(m.get_value((4, 1)).unwrap().to_string(), "1");
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test --manifest-path src-tauri/Cargo.toml excel_gen` → compile FAIL.

- [ ] **Step 3: Implement**

```rust
use crate::error::{AppError, AppResult};
use rust_xlsxwriter::{Formula, Workbook};
use std::collections::BTreeMap;
use std::path::Path;

pub const LAYOUT_VERSION: i64 = 1;

#[derive(Debug, Clone)]
pub struct WbMeta {
    pub run_id: i64,
    pub view_id: i64,
    pub kind: String,
    pub generated_at: String,
}

#[derive(Debug, Clone)]
pub struct GenAsset {
    pub asset_id: i64,
    pub asset_class_id: i64,
    pub class_name: String,
    pub label: String,
    pub bdp_security: String,
}

#[derive(Debug, Clone)]
pub struct GenField {
    pub field_id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
}

pub fn sanitize_sheet_name(raw: &str) -> String {
    let cleaned: String = raw.chars()
        .filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
        .take(31)
        .collect();
    if cleaned.trim().is_empty() { "Sheet".to_string() } else { cleaned }
}

fn write_meta(wb: &mut Workbook, meta: &WbMeta) -> AppResult<()> {
    let s = wb.add_worksheet().set_name("META").map_err(|e| AppError::Excel(e.to_string()))?;
    let rows: [(&str, String); 5] = [
        ("run_id", meta.run_id.to_string()),
        ("view_id", meta.view_id.to_string()),
        ("kind", meta.kind.clone()),
        ("generated_at", meta.generated_at.clone()),
        ("layout_version", LAYOUT_VERSION.to_string()),
    ];
    for (i, (k, v)) in rows.iter().enumerate() {
        s.write_string(i as u32, 0, *k).map_err(|e| AppError::Excel(e.to_string()))?;
        s.write_string(i as u32, 1, v).map_err(|e| AppError::Excel(e.to_string()))?;
    }
    s.set_hidden(true);
    Ok(())
}

pub fn generate_eod_workbook(
    path: &Path, meta: &WbMeta, assets: &[GenAsset], fields: &[GenField],
) -> AppResult<()> {
    if assets.is_empty() {
        return Err(AppError::Validation("view has no active assets".into()));
    }
    let mut wb = Workbook::new();

    // group by class, preserving stable order via BTreeMap on class id
    let mut by_class: BTreeMap<i64, (String, Vec<&GenAsset>)> = BTreeMap::new();
    for a in assets {
        by_class.entry(a.asset_class_id)
            .or_insert_with(|| (a.class_name.clone(), Vec::new()))
            .1.push(a);
    }

    for (class_id, (class_name, class_assets)) in &by_class {
        let class_fields: Vec<&GenField> =
            fields.iter().filter(|f| f.asset_class_id == *class_id).collect();
        if class_fields.is_empty() {
            return Err(AppError::Validation(
                format!("no fields configured for asset class '{class_name}'")));
        }
        let sheet = wb.add_worksheet()
            .set_name(sanitize_sheet_name(class_name))
            .map_err(|e| AppError::Excel(e.to_string()))?;
        sheet.write_string(0, 0, "SECURITY").map_err(|e| AppError::Excel(e.to_string()))?;
        for (ci, f) in class_fields.iter().enumerate() {
            sheet.write_string(0, (ci + 1) as u16, &f.mnemonic)
                .map_err(|e| AppError::Excel(e.to_string()))?;
        }
        for (ri, a) in class_assets.iter().enumerate() {
            let row = (ri + 1) as u32;
            sheet.write_string(row, 0, &a.bdp_security)
                .map_err(|e| AppError::Excel(e.to_string()))?;
            for (ci, f) in class_fields.iter().enumerate() {
                let formula = format!("=BDP($A{},\"{}\")", row + 1, f.mnemonic);
                sheet.write_formula(row, (ci + 1) as u16, Formula::new(formula))
                    .map_err(|e| AppError::Excel(e.to_string()))?;
            }
        }
    }

    write_meta(&mut wb, meta)?;
    wb.save(path).map_err(|e| AppError::Excel(e.to_string()))?;
    Ok(())
}
```

(If the installed rust_xlsxwriter version's `set_hidden`/`write_*` signatures differ, adapt the calls — the test, not the exact API line, is the contract.)

- [ ] **Step 4: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml excel_gen` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src
git commit -m "feat: EOD workbook generation with BDP formulas and META sheet"
```

---

### Task 6: Backfill workbook generation (`excel_gen.rs`, BDH)

**Files:**
- Modify: `src-tauri/src/excel_gen.rs`
- Test: unit tests in-module

**Interfaces:**
- Consumes: Task 5 structs.
- Produces:
  - `pub fn bdh_sheet_name(asset_id: i64) -> String` — returns `"A<asset_id>"` (e.g. `A42`); guaranteed unique and valid.
  - `pub fn generate_backfill_workbook(path: &Path, meta: &WbMeta, assets: &[GenAsset], fields: &[GenField], start: chrono::NaiveDate, end: chrono::NaiveDate) -> AppResult<()>`

**Layout (spec §4 stage 1, backfill):** still ONE workbook. One sheet per asset named `A<asset_id>`. Per sheet: `A1="asset_id"`, `B1=<id>`; `A2="security"`, `B2=<bdp_security>`; `A3="fields"`, `B3=<comma-joined mnemonics of the asset's class, in field order>`; row 5 (`A5`) holds the single spilling formula `=BDH("<security>","<fields>","<YYYYMMDD start>","<YYYYMMDD end>","Dates=S")`. BDH spills dates in column A from row 5 downward and one column per field to the right. Rows 1–3 let the reader verify identity and column order without guessing.

- [ ] **Step 1: Write failing tests** (append to `excel_gen.rs` tests module)

```rust
    #[test]
    fn backfill_workbook_one_sheet_per_asset_single_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf.xlsx");
        let (assets, fields) = sample();
        let meta = WbMeta { run_id: 8, view_id: 3, kind: "backfill".into(),
                            generated_at: "2026-08-13T10:00:00".into() };
        let start = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        generate_backfill_workbook(&path, &meta, &assets, &fields, start, end).unwrap();

        let mut wb: Xlsx<_> = open_workbook(&path).unwrap();
        let names = wb.sheet_names().to_vec();
        // one sheet per asset + META — all inside the single workbook
        assert!(names.contains(&"A1".to_string()));
        assert!(names.contains(&"A2".to_string()));
        assert!(names.contains(&"A3".to_string()));
        assert!(names.contains(&"META".to_string()));

        let r = wb.worksheet_range("A1").unwrap();
        assert_eq!(r.get_value((1, 1)).unwrap().to_string(), "AAPL US Equity");
        assert_eq!(r.get_value((2, 1)).unwrap().to_string(), "PX_LAST,PX_VOLUME");

        let f = wb.worksheet_formula("A1").unwrap();
        let cell = f.get_value((4, 0)).unwrap().to_string();
        assert!(cell.contains("BDH(\"AAPL US Equity\",\"PX_LAST,PX_VOLUME\",\"20260701\",\"20260731\",\"Dates=S\")"),
                "got formula: {cell}");
    }

    #[test]
    fn backfill_rejects_reversed_range() {
        let dir = tempfile::tempdir().unwrap();
        let (assets, fields) = sample();
        let meta = WbMeta { run_id: 8, view_id: 3, kind: "backfill".into(),
                            generated_at: "t".into() };
        let start = chrono::NaiveDate::from_ymd_opt(2026, 7, 31).unwrap();
        let end = chrono::NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(generate_backfill_workbook(&dir.path().join("x.xlsx"),
                &meta, &assets, &fields, start, end).is_err());
    }
```

- [ ] **Step 2: Run to verify failure** — compile FAIL (`generate_backfill_workbook` missing).

- [ ] **Step 3: Implement** (append to `excel_gen.rs`)

```rust
pub fn bdh_sheet_name(asset_id: i64) -> String {
    format!("A{asset_id}")
}

pub fn generate_backfill_workbook(
    path: &Path, meta: &WbMeta, assets: &[GenAsset], fields: &[GenField],
    start: chrono::NaiveDate, end: chrono::NaiveDate,
) -> AppResult<()> {
    if assets.is_empty() {
        return Err(AppError::Validation("view has no active assets".into()));
    }
    if start > end {
        return Err(AppError::Validation("backfill start date after end date".into()));
    }
    let (s, e) = (start.format("%Y%m%d").to_string(), end.format("%Y%m%d").to_string());
    let mut wb = Workbook::new();

    for a in assets {
        let mnemonics: Vec<&str> = fields.iter()
            .filter(|f| f.asset_class_id == a.asset_class_id)
            .map(|f| f.mnemonic.as_str())
            .collect();
        if mnemonics.is_empty() {
            return Err(AppError::Validation(
                format!("no fields configured for asset '{}'", a.label)));
        }
        let joined = mnemonics.join(",");
        let sheet = wb.add_worksheet()
            .set_name(bdh_sheet_name(a.asset_id))
            .map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(0, 0, "asset_id").map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(0, 1, a.asset_id.to_string()).map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(1, 0, "security").map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(1, 1, &a.bdp_security).map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(2, 0, "fields").map_err(|er| AppError::Excel(er.to_string()))?;
        sheet.write_string(2, 1, &joined).map_err(|er| AppError::Excel(er.to_string()))?;
        let formula = format!(
            "=BDH(\"{}\",\"{}\",\"{}\",\"{}\",\"Dates=S\")",
            a.bdp_security, joined, s, e);
        sheet.write_formula(4, 0, Formula::new(formula))
            .map_err(|er| AppError::Excel(er.to_string()))?;
    }

    write_meta(&mut wb, meta)?;
    wb.save(path).map_err(|er| AppError::Excel(er.to_string()))?;
    Ok(())
}
```

- [ ] **Step 4: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml excel_gen` → all PASS (both tasks' tests).

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src
git commit -m "feat: backfill workbook generation with per-asset BDH sheets"
```

---

### Task 7: Excel COM refresh driver (`scripts/refresh.ps1` + `refresh_driver.rs`)

**Files:**
- Create: `src-tauri/scripts/refresh.ps1`
- Create: `src-tauri/src/refresh_driver.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod refresh_driver;`)
- Test: unit tests in-module (arg building + status parsing, no Excel); manual dry-run on the real machine

**Interfaces:**
- Consumes: `AppError::Refresh` (Task 2).
- Produces:
  - `pub struct RefreshStatus { pub status: String, pub seconds: f64, pub detail: String }` (derives `serde::Deserialize, Debug, Clone`)
  - `pub fn build_ps_args(script: &Path, workbook: &Path, timeout_s: u32, dry_run: bool) -> Vec<String>`
  - `pub fn parse_status(stdout: &str) -> Option<RefreshStatus>` — parses the LAST line of stdout as JSON.
  - `pub async fn run_refresh(script: &Path, workbook: &Path, timeout_s: u32, dry_run: bool) -> AppResult<RefreshStatus>` — spawns `powershell.exe`, maps exit code `0` → Ok, `2`/`3`/other → `AppError::Refresh { code, detail }` with stderr + last stdout line as detail. **The retry-once-with-doubled-timeout policy lives in the orchestrator (Task 11), not here.**

**This script is the only COM code in the project (spec §4 stage 2). It must stay standalone-debuggable:** `powershell -NoProfile -ExecutionPolicy Bypass -File refresh.ps1 -WorkbookPath C:\x.xlsx -TimeoutSeconds 60 -DryRun` from any terminal.

- [ ] **Step 1: Write `refresh.ps1` in full**

```powershell
param(
    [Parameter(Mandatory=$true)][string]$WorkbookPath,
    [int]$TimeoutSeconds = 600,
    [switch]$DryRun
)
# Exit codes: 0 ok, 2 timeout, 3 excel/COM error. Last stdout line = JSON status.
$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Win32 {
    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr hWnd, out uint pid);
}
"@

$excel = $null; $book = $null; $excelPid = 0
$sw = [System.Diagnostics.Stopwatch]::StartNew()
$exit = 3; $status = 'excel-error'; $detail = ''

function Test-StillRequesting($wb) {
    foreach ($sheet in $wb.Worksheets) {
        $used = $sheet.UsedRange
        if ($null -ne $used) {
            $hit = $used.Find('Requesting Data')
            if ($null -ne $hit) { return $true }
        }
    }
    return $false
}

try {
    if (-not (Test-Path $WorkbookPath)) { throw "workbook not found: $WorkbookPath" }
    $excel = New-Object -ComObject Excel.Application
    $excel.Visible = $false
    $excel.DisplayAlerts = $false
    [void][Win32]::GetWindowThreadProcessId([IntPtr]$excel.Hwnd, [ref]$excelPid)

    $book = $excel.Workbooks.Open((Resolve-Path $WorkbookPath).Path)

    if (-not $DryRun) {
        # Give the Bloomberg add-in time to load, then force the static refresh.
        Start-Sleep -Seconds 15
        try { $excel.Run('RefreshAllStaticData') } catch { $detail = "RefreshAllStaticData: $_" }

        while (Test-StillRequesting $book) {
            if ($sw.Elapsed.TotalSeconds -gt $TimeoutSeconds) {
                $exit = 2; $status = 'timeout'
                $detail = "still requesting after $TimeoutSeconds s"
                throw 'timeout'
            }
            Start-Sleep -Seconds 5
        }
    }

    $book.Save()
    $exit = 0; $status = 'ok'; $detail = ''
}
catch {
    if ($exit -ne 2) { $exit = 3; $status = 'excel-error'; if (-not $detail) { $detail = "$_" } }
}
finally {
    try { if ($null -ne $book) { $book.Close($false) } } catch {}
    try { if ($null -ne $excel) { $excel.Quit() } } catch {}
    try {
        if ($null -ne $excel) {
            [void][System.Runtime.InteropServices.Marshal]::ReleaseComObject($excel)
        }
    } catch {}
    # Kill the exact Excel process we started if it survived Quit().
    if ($excelPid -gt 0) {
        $p = Get-Process -Id $excelPid -ErrorAction SilentlyContinue
        if ($null -ne $p) { try { $p.Kill() } catch {} }
    }
}

@{ status = $status; seconds = [math]::Round($sw.Elapsed.TotalSeconds, 1); detail = $detail } |
    ConvertTo-Json -Compress
exit $exit
```

- [ ] **Step 2: Sanity-check the script parses**

Run: `powershell -NoProfile -Command "Get-Command -Syntax -Name .\src-tauri\scripts\refresh.ps1"` (or just run it against a nonexistent path).
Run: `powershell -NoProfile -ExecutionPolicy Bypass -File src-tauri\scripts\refresh.ps1 -WorkbookPath C:\nope.xlsx`
Expected: exit code 3, last line is compact JSON with `"status":"excel-error"` — verify with `echo $LASTEXITCODE`.

- [ ] **Step 3: Write failing Rust unit tests** (in `refresh_driver.rs`)

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn args_include_paths_timeout_and_flags() {
        let a = build_ps_args(Path::new("C:\\app\\refresh.ps1"),
                              Path::new("C:\\pending\\wb.xlsx"), 300, true);
        assert_eq!(a[0], "-NoProfile");
        assert!(a.contains(&"-ExecutionPolicy".to_string()));
        assert!(a.contains(&"-File".to_string()));
        assert!(a.contains(&"C:\\app\\refresh.ps1".to_string()));
        assert!(a.contains(&"-WorkbookPath".to_string()));
        assert!(a.contains(&"C:\\pending\\wb.xlsx".to_string()));
        assert!(a.contains(&"-TimeoutSeconds".to_string()));
        assert!(a.contains(&"300".to_string()));
        assert!(a.contains(&"-DryRun".to_string()));
        let b = build_ps_args(Path::new("s.ps1"), Path::new("w.xlsx"), 300, false);
        assert!(!b.contains(&"-DryRun".to_string()));
    }

    #[test]
    fn parses_last_stdout_line_as_status() {
        let out = "noise from addin\n{\"status\":\"ok\",\"seconds\":42.5,\"detail\":\"\"}\n";
        let s = parse_status(out).unwrap();
        assert_eq!(s.status, "ok");
        assert_eq!(s.seconds, 42.5);
        assert!(parse_status("no json here").is_none());
    }
}
```

- [ ] **Step 4: Run to verify failure** — compile FAIL.

- [ ] **Step 5: Implement `refresh_driver.rs`**

```rust
use crate::error::{AppError, AppResult};
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
pub struct RefreshStatus {
    pub status: String,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub detail: String,
}

pub fn build_ps_args(script: &Path, workbook: &Path, timeout_s: u32, dry_run: bool) -> Vec<String> {
    let mut args = vec![
        "-NoProfile".into(),
        "-ExecutionPolicy".into(), "Bypass".into(),
        "-File".into(), script.to_string_lossy().into_owned(),
        "-WorkbookPath".into(), workbook.to_string_lossy().into_owned(),
        "-TimeoutSeconds".into(), timeout_s.to_string(),
    ];
    if dry_run {
        args.push("-DryRun".into());
    }
    args
}

pub fn parse_status(stdout: &str) -> Option<RefreshStatus> {
    stdout.lines().rev()
        .find_map(|l| serde_json::from_str::<RefreshStatus>(l.trim()).ok())
}

pub async fn run_refresh(
    script: &Path, workbook: &Path, timeout_s: u32, dry_run: bool,
) -> AppResult<RefreshStatus> {
    let out = tokio::process::Command::new("powershell.exe")
        .args(build_ps_args(script, workbook, timeout_s, dry_run))
        .output()
        .await?;
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    let code = out.status.code().unwrap_or(-1);
    if code == 0 {
        parse_status(&stdout).ok_or_else(|| AppError::Refresh {
            code: 0, detail: "exit 0 but no JSON status on stdout".into() })
    } else {
        let detail = parse_status(&stdout)
            .map(|s| s.detail)
            .filter(|d| !d.is_empty())
            .unwrap_or(stderr);
        Err(AppError::Refresh { code, detail })
    }
}
```

- [ ] **Step 6: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml refresh_driver` → PASS.

- [ ] **Step 7: Manual dry-run check (real machine, no Bloomberg needed)**

Generate any workbook (e.g. run the Task 5 test, keep its tempfile, or save an empty .xlsx), then:

```powershell
powershell -NoProfile -ExecutionPolicy Bypass -File src-tauri\scripts\refresh.ps1 -WorkbookPath <path> -TimeoutSeconds 60 -DryRun
echo $LASTEXITCODE
```

Expected: `0`, JSON `"status":"ok"`, no `EXCEL.EXE` left in Task Manager.

- [ ] **Step 8: Commit**

```bash
git add src-tauri/scripts src-tauri/src
git commit -m "feat: powershell COM refresh driver with pid-exact cleanup and rust spawn wrapper"
```

---

### Task 8: Read & validate refreshed workbooks (`excel_read.rs`)

**Files:**
- Create: `src-tauri/src/excel_read.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod excel_read;`)
- Test: unit tests in-module using fixture workbooks written with rust_xlsxwriter (plain values simulate post-refresh cached values; no Excel, no DB)

**Interfaces:**
- Consumes: `GenAsset`, `sanitize_sheet_name`, `bdh_sheet_name`, `LAYOUT_VERSION` (Tasks 5–6).
- Produces:
  - `pub enum CellValue { Num(f64), Text(String) }` (derives `Debug, Clone, PartialEq`)
  - `pub struct FieldSpec { pub field_id: i64, pub asset_class_id: i64, pub mnemonic: String, pub value_kind: String }` — the read-side twin of `GenField`, carrying `value_kind`. The orchestrator builds both from the same `FieldDef` rows.
  - `pub struct ObsCell { pub asset_id: i64, pub field_id: i64, pub obs_date: chrono::NaiveDate, pub value: CellValue }`
  - `pub struct CellProblem { pub asset_id: Option<i64>, pub field_id: Option<i64>, pub obs_date: Option<chrono::NaiveDate>, pub code: String, pub detail: String }`
  - `pub struct ReadOutcome { pub cells: Vec<ObsCell>, pub problems: Vec<CellProblem> }`
  - `pub fn classify_cell(data: &calamine::Data, value_kind: &str) -> Result<CellValue, (String, String)>` — pure; `Err` is `(code, detail)`.
  - `pub fn excel_serial_to_date(serial: f64) -> chrono::NaiveDate`
  - `pub fn read_meta(path: &Path) -> AppResult<MetaRead>` where `pub struct MetaRead { pub run_id: i64, pub view_id: i64, pub kind: String, pub layout_version: i64 }`
  - `pub fn read_eod_workbook(path: &Path, expected_run_id: i64, assets: &[GenAsset], fields: &[FieldSpec], obs_date: chrono::NaiveDate) -> AppResult<ReadOutcome>`
  - `pub fn read_backfill_workbook(path: &Path, expected_run_id: i64, assets: &[GenAsset], fields: &[FieldSpec]) -> AppResult<ReadOutcome>`

**Classification rules (spec §4 stage 3):** error cells become problems, never observations. Codes: `requesting` (contains `Requesting Data` — should not survive the driver, but classify anyway), `invalid_security` (`#N/A Invalid Security`), `field_not_applicable` (`#N/A Field Not Applicable`), `na` (any other string starting `#N/A` or calamine `Data::Error`), `empty`, `type_mismatch`. META `run_id`/`layout_version` mismatch fails the whole read with `AppError::Validation` **before any ingest**.

- [ ] **Step 1: Write failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::excel_gen::GenAsset;
    use calamine::Data;
    use chrono::NaiveDate;
    use rust_xlsxwriter::Workbook;

    #[test]
    fn classify_numeric_and_errors() {
        assert_eq!(classify_cell(&Data::Float(231.4), "numeric"), Ok(CellValue::Num(231.4)));
        assert_eq!(classify_cell(&Data::Int(5), "numeric"), Ok(CellValue::Num(5.0)));
        assert_eq!(classify_cell(&Data::String("hello".into()), "text"),
                   Ok(CellValue::Text("hello".into())));
        assert_eq!(classify_cell(&Data::Empty, "numeric").unwrap_err().0, "empty");
        assert_eq!(classify_cell(&Data::String("#N/A Invalid Security".into()), "numeric")
                   .unwrap_err().0, "invalid_security");
        assert_eq!(classify_cell(&Data::String("#N/A Field Not Applicable".into()), "numeric")
                   .unwrap_err().0, "field_not_applicable");
        assert_eq!(classify_cell(&Data::String("#N/A Requesting Data...".into()), "numeric")
                   .unwrap_err().0, "requesting");
        assert_eq!(classify_cell(&Data::String("not a number".into()), "numeric")
                   .unwrap_err().0, "type_mismatch");
    }

    #[test]
    fn classify_date_kinds() {
        // Excel serial for 2026-07-01
        let d = classify_cell(&Data::Float(46204.0), "date").unwrap();
        assert_eq!(d, CellValue::Text("2026-07-01".into()));
        assert_eq!(excel_serial_to_date(46204.0),
                   NaiveDate::from_ymd_opt(2026, 7, 1).unwrap());
    }

    fn fixture_assets() -> (Vec<GenAsset>, Vec<FieldSpec>) {
        let assets = vec![GenAsset {
            asset_id: 1, asset_class_id: 10, class_name: "Equity".into(),
            label: "Apple".into(), bdp_security: "AAPL US Equity".into() }];
        let fields = vec![
            FieldSpec { field_id: 100, asset_class_id: 10,
                        mnemonic: "PX_LAST".into(), value_kind: "numeric".into() },
            FieldSpec { field_id: 101, asset_class_id: 10,
                        mnemonic: "PX_VOLUME".into(), value_kind: "numeric".into() },
        ];
        (assets, fields)
    }

    fn write_meta_fixture(wb: &mut Workbook, run_id: i64) {
        let s = wb.add_worksheet().set_name("META").unwrap();
        for (i, (k, v)) in [("run_id", run_id.to_string()), ("view_id", "3".into()),
                            ("kind", "eod".into()), ("generated_at", "t".into()),
                            ("layout_version", "1".into())].iter().enumerate() {
            s.write_string(i as u32, 0, *k).unwrap();
            s.write_string(i as u32, 1, v).unwrap();
        }
    }

    #[test]
    fn eod_read_mixes_values_and_problems() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("refreshed.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("Equity").unwrap();
        s.write_string(0, 0, "SECURITY").unwrap();
        s.write_string(0, 1, "PX_LAST").unwrap();
        s.write_string(0, 2, "PX_VOLUME").unwrap();
        s.write_string(1, 0, "AAPL US Equity").unwrap();
        s.write_number(1, 1, 231.4).unwrap();
        s.write_string(1, 2, "#N/A Invalid Security").unwrap();
        write_meta_fixture(&mut wb, 7);
        wb.save(&path).unwrap();

        let (assets, fields) = fixture_assets();
        let d = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        let out = read_eod_workbook(&path, 7, &assets, &fields, d).unwrap();
        assert_eq!(out.cells.len(), 1);
        assert_eq!(out.cells[0].asset_id, 1);
        assert_eq!(out.cells[0].field_id, 100);
        assert_eq!(out.cells[0].obs_date, d);
        assert_eq!(out.cells[0].value, CellValue::Num(231.4));
        assert_eq!(out.problems.len(), 1);
        assert_eq!(out.problems[0].code, "invalid_security");
        assert_eq!(out.problems[0].field_id, Some(101));
    }

    #[test]
    fn eod_read_rejects_wrong_run_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stale.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("Equity").unwrap();
        s.write_string(0, 0, "SECURITY").unwrap();
        write_meta_fixture(&mut wb, 999);
        wb.save(&path).unwrap();
        let (assets, fields) = fixture_assets();
        let d = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
        assert!(read_eod_workbook(&path, 7, &assets, &fields, d).is_err());
    }

    #[test]
    fn backfill_read_walks_bdh_spill() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bf.xlsx");
        let mut wb = Workbook::new();
        let s = wb.add_worksheet().set_name("A1").unwrap();
        s.write_string(0, 0, "asset_id").unwrap();
        s.write_string(0, 1, "1").unwrap();
        s.write_string(1, 0, "security").unwrap();
        s.write_string(1, 1, "AAPL US Equity").unwrap();
        s.write_string(2, 0, "fields").unwrap();
        s.write_string(2, 1, "PX_LAST,PX_VOLUME").unwrap();
        // simulated BDH spill from row 5: date serial | px_last | px_volume
        s.write_number(4, 0, 46204.0).unwrap();  // 2026-07-01
        s.write_number(4, 1, 230.0).unwrap();
        s.write_number(4, 2, 1000.0).unwrap();
        s.write_number(5, 0, 46205.0).unwrap();  // 2026-07-02
        s.write_number(5, 1, 231.0).unwrap();
        s.write_number(5, 2, 1100.0).unwrap();
        write_meta_fixture(&mut wb, 8);
        wb.save(&path).unwrap();

        let (assets, fields) = fixture_assets();
        let out = read_backfill_workbook(&path, 8, &assets, &fields).unwrap();
        assert_eq!(out.cells.len(), 4);  // 2 days x 2 fields
        assert!(out.problems.is_empty());
        let d1 = NaiveDate::from_ymd_opt(2026, 7, 1).unwrap();
        assert!(out.cells.iter().any(|c|
            c.obs_date == d1 && c.field_id == 100 && c.value == CellValue::Num(230.0)));
    }
}
```

- [ ] **Step 2: Run to verify failure** — compile FAIL.

- [ ] **Step 3: Implement `excel_read.rs`**

```rust
use crate::error::{AppError, AppResult};
use crate::excel_gen::{bdh_sheet_name, sanitize_sheet_name, GenAsset, LAYOUT_VERSION};
use calamine::{open_workbook, Data, Reader, Xlsx};
use chrono::NaiveDate;
use std::collections::HashMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub enum CellValue { Num(f64), Text(String) }

#[derive(Debug, Clone)]
pub struct FieldSpec {
    pub field_id: i64,
    pub asset_class_id: i64,
    pub mnemonic: String,
    pub value_kind: String,
}

#[derive(Debug, Clone)]
pub struct ObsCell {
    pub asset_id: i64,
    pub field_id: i64,
    pub obs_date: NaiveDate,
    pub value: CellValue,
}

#[derive(Debug, Clone)]
pub struct CellProblem {
    pub asset_id: Option<i64>,
    pub field_id: Option<i64>,
    pub obs_date: Option<NaiveDate>,
    pub code: String,
    pub detail: String,
}

#[derive(Debug, Default)]
pub struct ReadOutcome {
    pub cells: Vec<ObsCell>,
    pub problems: Vec<CellProblem>,
}

#[derive(Debug)]
pub struct MetaRead {
    pub run_id: i64,
    pub view_id: i64,
    pub kind: String,
    pub layout_version: i64,
}

pub fn excel_serial_to_date(serial: f64) -> NaiveDate {
    NaiveDate::from_ymd_opt(1899, 12, 30).unwrap()
        + chrono::Duration::days(serial as i64)
}

pub fn classify_cell(data: &Data, value_kind: &str) -> Result<CellValue, (String, String)> {
    match data {
        Data::Empty => Err(("empty".into(), "empty cell".into())),
        Data::Error(e) => Err(("na".into(), format!("{e:?}"))),
        Data::String(s) => {
            let t = s.trim();
            if t.contains("Requesting Data") {
                Err(("requesting".into(), t.into()))
            } else if t.starts_with("#N/A Invalid Security") {
                Err(("invalid_security".into(), t.into()))
            } else if t.starts_with("#N/A Field Not Applicable") {
                Err(("field_not_applicable".into(), t.into()))
            } else if t.starts_with("#N/A") || t.starts_with("#NAME") || t.starts_with("#VALUE") {
                Err(("na".into(), t.into()))
            } else if t.is_empty() {
                Err(("empty".into(), "blank string".into()))
            } else {
                match value_kind {
                    "numeric" => t.replace(',', "").parse::<f64>()
                        .map(CellValue::Num)
                        .map_err(|_| ("type_mismatch".into(),
                                      format!("expected numeric, got '{t}'"))),
                    "date" => NaiveDate::parse_from_str(t, "%Y-%m-%d")
                        .or_else(|_| NaiveDate::parse_from_str(t, "%m/%d/%Y"))
                        .map(|d| CellValue::Text(d.format("%Y-%m-%d").to_string()))
                        .map_err(|_| ("type_mismatch".into(),
                                      format!("expected date, got '{t}'"))),
                    _ => Ok(CellValue::Text(t.into())),
                }
            }
        }
        Data::Float(f) => match value_kind {
            "numeric" => Ok(CellValue::Num(*f)),
            "date" => Ok(CellValue::Text(
                excel_serial_to_date(*f).format("%Y-%m-%d").to_string())),
            _ => Ok(CellValue::Text(f.to_string())),
        },
        Data::Int(i) => match value_kind {
            "numeric" => Ok(CellValue::Num(*i as f64)),
            "date" => Ok(CellValue::Text(
                excel_serial_to_date(*i as f64).format("%Y-%m-%d").to_string())),
            _ => Ok(CellValue::Text(i.to_string())),
        },
        Data::DateTime(dt) => {
            let d = excel_serial_to_date(dt.as_f64());
            match value_kind {
                "numeric" => Err(("type_mismatch".into(), "date where numeric expected".into())),
                _ => Ok(CellValue::Text(d.format("%Y-%m-%d").to_string())),
            }
        }
        other => Err(("na".into(), format!("unhandled cell {other:?}"))),
    }
}

pub fn read_meta(path: &Path) -> AppResult<MetaRead> {
    let mut wb: Xlsx<_> = open_workbook(path).map_err(|e| AppError::Excel(e.to_string()))?;
    let r = wb.worksheet_range("META")
        .map_err(|e| AppError::Validation(format!("META sheet missing: {e}")))?;
    let mut kv = HashMap::new();
    for row in r.rows() {
        if row.len() >= 2 {
            kv.insert(row[0].to_string(), row[1].to_string());
        }
    }
    let get_i64 = |k: &str| -> AppResult<i64> {
        kv.get(k).and_then(|v| v.parse().ok())
            .ok_or_else(|| AppError::Validation(format!("META missing {k}")))
    };
    Ok(MetaRead {
        run_id: get_i64("run_id")?,
        view_id: get_i64("view_id")?,
        kind: kv.get("kind").cloned().unwrap_or_default(),
        layout_version: get_i64("layout_version")?,
    })
}

fn check_meta(path: &Path, expected_run_id: i64) -> AppResult<()> {
    let m = read_meta(path)?;
    if m.run_id != expected_run_id {
        return Err(AppError::Validation(
            format!("META run_id {} != expected {expected_run_id}", m.run_id)));
    }
    if m.layout_version != LAYOUT_VERSION {
        return Err(AppError::Validation(
            format!("META layout_version {} != {LAYOUT_VERSION}", m.layout_version)));
    }
    Ok(())
}

pub fn read_eod_workbook(
    path: &Path, expected_run_id: i64, assets: &[GenAsset],
    fields: &[FieldSpec], obs_date: NaiveDate,
) -> AppResult<ReadOutcome> {
    check_meta(path, expected_run_id)?;
    let mut wb: Xlsx<_> = open_workbook(path).map_err(|e| AppError::Excel(e.to_string()))?;
    let by_security: HashMap<&str, &GenAsset> =
        assets.iter().map(|a| (a.bdp_security.as_str(), a)).collect();
    let mut out = ReadOutcome::default();

    let mut classes: Vec<(i64, String)> = assets.iter()
        .map(|a| (a.asset_class_id, a.class_name.clone())).collect();
    classes.sort();
    classes.dedup();

    for (class_id, class_name) in classes {
        let sheet = sanitize_sheet_name(&class_name);
        let r = wb.worksheet_range(&sheet)
            .map_err(|e| AppError::Validation(format!("sheet '{sheet}' missing: {e}")))?;
        // header row -> FieldSpec per column
        let header: Vec<String> = r.rows().next()
            .map(|row| row.iter().map(|c| c.to_string()).collect())
            .unwrap_or_default();
        let col_fields: Vec<Option<&FieldSpec>> = header.iter().skip(1)
            .map(|m| fields.iter()
                .find(|f| f.asset_class_id == class_id && f.mnemonic == *m))
            .collect();
        for row in r.rows().skip(1) {
            let sec = row.first().map(|c| c.to_string()).unwrap_or_default();
            let Some(asset) = by_security.get(sec.as_str()) else {
                out.problems.push(CellProblem {
                    asset_id: None, field_id: None, obs_date: Some(obs_date),
                    code: "unknown_security".into(),
                    detail: format!("row security '{sec}' not in view") });
                continue;
            };
            for (ci, fspec) in col_fields.iter().enumerate() {
                let Some(f) = fspec else { continue };
                let cell = row.get(ci + 1).unwrap_or(&Data::Empty);
                match classify_cell(cell, &f.value_kind) {
                    Ok(v) => out.cells.push(ObsCell {
                        asset_id: asset.asset_id, field_id: f.field_id,
                        obs_date, value: v }),
                    Err((code, detail)) => out.problems.push(CellProblem {
                        asset_id: Some(asset.asset_id), field_id: Some(f.field_id),
                        obs_date: Some(obs_date), code, detail }),
                }
            }
        }
    }
    Ok(out)
}

pub fn read_backfill_workbook(
    path: &Path, expected_run_id: i64, assets: &[GenAsset], fields: &[FieldSpec],
) -> AppResult<ReadOutcome> {
    check_meta(path, expected_run_id)?;
    let mut wb: Xlsx<_> = open_workbook(path).map_err(|e| AppError::Excel(e.to_string()))?;
    let mut out = ReadOutcome::default();

    for a in assets {
        let sheet = bdh_sheet_name(a.asset_id);
        let r = wb.worksheet_range(&sheet)
            .map_err(|e| AppError::Validation(format!("sheet '{sheet}' missing: {e}")))?;
        let rows: Vec<_> = r.rows().collect();
        let joined = rows.get(2).and_then(|row| row.get(1))
            .map(|c| c.to_string()).unwrap_or_default();
        let col_fields: Vec<Option<&FieldSpec>> = joined.split(',')
            .map(|m| fields.iter()
                .find(|f| f.asset_class_id == a.asset_class_id && f.mnemonic == m.trim()))
            .collect();
        for row in rows.iter().skip(4) {
            let date_cell = row.first().unwrap_or(&Data::Empty);
            if matches!(date_cell, Data::Empty) { continue; }  // past end of spill
            let obs_date = match classify_cell(date_cell, "date") {
                Ok(CellValue::Text(d)) =>
                    NaiveDate::parse_from_str(&d, "%Y-%m-%d").unwrap(),
                _ => {
                    out.problems.push(CellProblem {
                        asset_id: Some(a.asset_id), field_id: None, obs_date: None,
                        code: "bad_date".into(),
                        detail: format!("unparseable BDH date cell {date_cell:?}") });
                    continue;
                }
            };
            for (ci, fspec) in col_fields.iter().enumerate() {
                let Some(f) = fspec else { continue };
                let cell = row.get(ci + 1).unwrap_or(&Data::Empty);
                match classify_cell(cell, &f.value_kind) {
                    Ok(v) => out.cells.push(ObsCell {
                        asset_id: a.asset_id, field_id: f.field_id,
                        obs_date, value: v }),
                    Err((code, detail)) => out.problems.push(CellProblem {
                        asset_id: Some(a.asset_id), field_id: Some(f.field_id),
                        obs_date: Some(obs_date), code, detail }),
                }
            }
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml excel_read` → all PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src
git commit -m "feat: workbook reader with per-cell classification and META verification"
```

---

### Task 9: Idempotent ingest (`ingest.rs`)

**Files:**
- Create: `src-tauri/src/ingest.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod ingest;`)
- Test: ignored DB tests in `src-tauri/tests/db_integration.rs`

**Interfaces:**
- Consumes: `ReadOutcome`, `ObsCell`, `CellValue`, `CellProblem` (Task 8).
- Produces:
  - `pub struct IngestSummary { pub upserted: u64, pub issues: u64 }` (derives `Serialize, Debug, Clone`)
  - `pub async fn ingest_outcome(pool: &PgPool, run_id: i64, outcome: &ReadOutcome) -> AppResult<IngestSummary>` — one transaction for the whole run (spec §4 stage 4): every valid cell upserted, every problem written to `ingest_issue` with `severity='warn'`; commit or rollback together.

- [ ] **Step 1: Implement `ingest.rs`**

```rust
use crate::error::AppResult;
use crate::excel_read::{CellValue, ReadOutcome};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize)]
pub struct IngestSummary {
    pub upserted: u64,
    pub issues: u64,
}

pub async fn ingest_outcome(pool: &PgPool, run_id: i64, outcome: &ReadOutcome)
    -> AppResult<IngestSummary> {
    let mut tx = pool.begin().await?;
    let mut upserted = 0u64;
    for c in &outcome.cells {
        let (num, text) = match &c.value {
            CellValue::Num(n) => (Some(*n), None),
            CellValue::Text(t) => (None, Some(t.clone())),
        };
        sqlx::query(
            "INSERT INTO observation
               (asset_id, field_id, obs_date, value_num, value_text, run_id)
             VALUES ($1,$2,$3,$4,$5,$6)
             ON CONFLICT (asset_id, field_id, obs_date) DO UPDATE
               SET value_num = EXCLUDED.value_num,
                   value_text = EXCLUDED.value_text,
                   run_id = EXCLUDED.run_id,
                   ingested_at = now()")
            .bind(c.asset_id).bind(c.field_id).bind(c.obs_date)
            .bind(num).bind(text).bind(run_id)
            .execute(&mut *tx).await?;
        upserted += 1;
    }
    for p in &outcome.problems {
        sqlx::query(
            "INSERT INTO ingest_issue
               (run_id, asset_id, field_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,$4,'warn',$5,$6)")
            .bind(run_id).bind(p.asset_id).bind(p.field_id).bind(p.obs_date)
            .bind(&p.code).bind(&p.detail)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(IngestSummary { upserted, issues: outcome.problems.len() as u64 })
}
```

- [ ] **Step 2: Write the idempotency test** (append to `tests/db_integration.rs`)

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn ingest_twice_converges_no_duplicates() {
    use getbloomdata_lib::{db, fields, ingest, registry, views};
    use getbloomdata_lib::excel_read::{CellValue, ObsCell, ReadOutcome};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, "EquityT9", "t").await.unwrap();
    let f = fields::create_field(&pool, class.id, "PX_LAST_T9", "px", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "T9".into(), id_kind: "ticker".into(),
        ticker: Some("T9 US".into()), isin: None, yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, "t9-view", "").await.unwrap();
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ingesting') RETURNING id")
        .bind(v.id).fetch_one(&pool).await.unwrap();

    let d = chrono::NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();
    let mk = |val: f64| ReadOutcome {
        cells: vec![ObsCell { asset_id: a.id, field_id: f.id,
                              obs_date: d, value: CellValue::Num(val) }],
        problems: vec![],
    };
    ingest::ingest_outcome(&pool, run_id, &mk(100.0)).await.unwrap();
    ingest::ingest_outcome(&pool, run_id, &mk(101.5)).await.unwrap();  // re-run: update, not dup

    let (count, val): (i64, f64) = sqlx::query_as(
        "SELECT count(*)::bigint, max(value_num)
         FROM observation WHERE asset_id = $1 AND field_id = $2")
        .bind(a.id).bind(f.id).fetch_one(&pool).await.unwrap();
    assert_eq!(count, 1);
    assert_eq!(val, 101.5);
}
```

- [ ] **Step 3: Run** — `cargo test --manifest-path src-tauri/Cargo.toml` compiles green; with DB up, `cargo test -- --ignored` → PASS.

- [ ] **Step 4: Commit**

```bash
git add src-tauri/src src-tauri/tests
git commit -m "feat: transactional idempotent ingest with per-cell issue rows"
```

---

### Task 10: Hit budget (`budget.rs`)

**Files:**
- Create: `src-tauri/src/budget.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod budget;`)
- Test: unit tests in-module (pure math); ledger functions exercised by Task 11's DB test

**Interfaces:**
- Consumes: `GenAsset` (Task 5), `FieldSpec` (Task 8).
- Produces:
  - `pub const DEFAULT_SOFT_LIMIT: i64 = 100_000;` (hard = soft × 2, per spec §7)
  - `pub enum BudgetLevel { Ok, SoftWarn, HardConfirm }` (derives `Serialize, Debug, Clone, PartialEq`)
  - `pub fn weekdays_between(start: NaiveDate, end: NaiveDate) -> i64` (inclusive)
  - `pub fn estimate_eod_hits(assets: &[GenAsset], fields: &[FieldSpec]) -> i64` — Σ per asset of its class's field count (BDP ≈ 1 hit per security × field).
  - `pub fn estimate_backfill_hits(assets: &[GenAsset], fields: &[FieldSpec], start: NaiveDate, end: NaiveDate) -> i64` — eod estimate × weekdays in range (conservative upper bound: BDH ≈ 1 hit per security × field × returned day, and returned days ≤ weekdays).
  - `pub fn check_level(estimated: i64, today_total: i64, soft: i64) -> BudgetLevel`
  - `pub async fn today_hits(pool: &PgPool) -> AppResult<i64>` — `SELECT coalesce(sum(estimated_hits),0) FROM hit_ledger WHERE occurred_on = CURRENT_DATE`.
  - `pub async fn record_hits(pool: &PgPool, run_id: i64, estimated: i64) -> AppResult<()>`

- [ ] **Step 1: Write failing unit tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::excel_gen::GenAsset;
    use crate::excel_read::FieldSpec;
    use chrono::NaiveDate;

    fn fixture() -> (Vec<GenAsset>, Vec<FieldSpec>) {
        let mk_a = |id, class| GenAsset {
            asset_id: id, asset_class_id: class, class_name: format!("C{class}"),
            label: format!("A{id}"), bdp_security: format!("S{id} Equity") };
        let mk_f = |id, class, m: &str| FieldSpec {
            field_id: id, asset_class_id: class,
            mnemonic: m.into(), value_kind: "numeric".into() };
        // 2 equity assets x 3 equity fields + 1 index asset x 1 index field = 7
        (vec![mk_a(1, 10), mk_a(2, 10), mk_a(3, 20)],
         vec![mk_f(1, 10, "PX_LAST"), mk_f(2, 10, "PX_BID"), mk_f(3, 10, "PX_ASK"),
              mk_f(4, 20, "PX_LAST")])
    }

    #[test]
    fn eod_estimate_is_security_times_class_fields() {
        let (a, f) = fixture();
        assert_eq!(estimate_eod_hits(&a, &f), 7);
    }

    #[test]
    fn weekday_count_inclusive() {
        // Mon 2026-08-03 .. Fri 2026-08-14 = 10 weekdays
        let s = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        assert_eq!(weekdays_between(s, e), 10);
        // weekend-only range
        let sat = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();
        let sun = NaiveDate::from_ymd_opt(2026, 8, 9).unwrap();
        assert_eq!(weekdays_between(sat, sun), 0);
    }

    #[test]
    fn backfill_estimate_scales_by_weekdays() {
        let (a, f) = fixture();
        let s = NaiveDate::from_ymd_opt(2026, 8, 3).unwrap();
        let e = NaiveDate::from_ymd_opt(2026, 8, 14).unwrap();
        assert_eq!(estimate_backfill_hits(&a, &f, s, e), 70);
    }

    #[test]
    fn levels_use_soft_and_double_soft() {
        assert_eq!(check_level(1_000, 0, 100_000), BudgetLevel::Ok);
        assert_eq!(check_level(60_000, 50_000, 100_000), BudgetLevel::SoftWarn);
        assert_eq!(check_level(150_001, 50_000, 100_000), BudgetLevel::HardConfirm);
        // cumulative: today's ledger counts toward the thresholds
        assert_eq!(check_level(1, 200_000, 100_000), BudgetLevel::HardConfirm);
    }
}
```

- [ ] **Step 2: Run to verify failure** — compile FAIL.

- [ ] **Step 3: Implement `budget.rs`**

```rust
use crate::error::AppResult;
use crate::excel_gen::GenAsset;
use crate::excel_read::FieldSpec;
use chrono::{Datelike, NaiveDate, Weekday};
use serde::Serialize;
use sqlx::PgPool;

pub const DEFAULT_SOFT_LIMIT: i64 = 100_000;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub enum BudgetLevel { Ok, SoftWarn, HardConfirm }

pub fn weekdays_between(start: NaiveDate, end: NaiveDate) -> i64 {
    let mut d = start;
    let mut n = 0;
    while d <= end {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) {
            n += 1;
        }
        d += chrono::Duration::days(1);
    }
    n
}

pub fn estimate_eod_hits(assets: &[GenAsset], fields: &[FieldSpec]) -> i64 {
    assets.iter().map(|a|
        fields.iter().filter(|f| f.asset_class_id == a.asset_class_id).count() as i64
    ).sum()
}

pub fn estimate_backfill_hits(
    assets: &[GenAsset], fields: &[FieldSpec], start: NaiveDate, end: NaiveDate,
) -> i64 {
    estimate_eod_hits(assets, fields) * weekdays_between(start, end)
}

pub fn check_level(estimated: i64, today_total: i64, soft: i64) -> BudgetLevel {
    let projected = estimated + today_total;
    if projected > soft * 2 {
        BudgetLevel::HardConfirm
    } else if projected > soft {
        BudgetLevel::SoftWarn
    } else {
        BudgetLevel::Ok
    }
}

pub async fn today_hits(pool: &PgPool) -> AppResult<i64> {
    Ok(sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits),0)::bigint
         FROM hit_ledger WHERE occurred_on = CURRENT_DATE")
        .fetch_one(pool).await?)
}

pub async fn record_hits(pool: &PgPool, run_id: i64, estimated: i64) -> AppResult<()> {
    sqlx::query("INSERT INTO hit_ledger (run_id, estimated_hits) VALUES ($1,$2)")
        .bind(run_id).bind(estimated).execute(pool).await?;
    Ok(())
}
```

- [ ] **Step 4: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml budget` → PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src
git commit -m "feat: hit budget estimator, thresholds, and daily ledger"
```

---

### Task 11: Pipeline orchestrator (`orchestrator.rs`)

**Files:**
- Create: `src-tauri/src/orchestrator.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod orchestrator;`)
- Test: unit tests in-module (pure path/plumbing); ignored end-to-end test in `tests/db_integration.rs`

**Interfaces:**
- Consumes: everything from Tasks 3–10 (`views::view_assets/view_fields`, `excel_gen::*`, `refresh_driver::run_refresh`, `excel_read::*`, `ingest::ingest_outcome`, `budget::*`).
- Produces:
  - `pub struct PipelineConfig { pub data_dir: PathBuf, pub script_path: PathBuf, pub refresh_timeout_s: u32, pub soft_limit: i64, pub dry_run_refresh: bool }` (derives `Clone, Debug`; `dry_run_refresh` exists ONLY for tests/smoke — normal runs pass `false`)
  - `pub enum RunOutcome { Completed { run_id: i64, summary: IngestSummary }, NeedsConfirmation { estimated: i64, today_total: i64 } }` (derives `Serialize, Debug`)
  - `pub fn pending_path(data_dir: &Path, view_name: &str, date: NaiveDate) -> PathBuf` → `<data_dir>/pending/<view>_<YYYY-MM-DD>.xlsx`
  - `pub fn archive_path(data_dir: &Path, run_id: i64, view_name: &str, date: NaiveDate) -> PathBuf` → `<data_dir>/archive/<YYYY>/<MM>/run_<run_id>_<view>_<YYYY-MM-DD>.xlsx` (spec §2.3 naming, exactly)
  - `pub async fn run_eod(pool: &PgPool, cfg: &PipelineConfig, view_id: i64, trigger: &str, obs_date: NaiveDate, confirmed: bool) -> AppResult<RunOutcome>`
  - `pub async fn run_backfill(pool: &PgPool, cfg: &PipelineConfig, view_id: i64, start: NaiveDate, end: NaiveDate, confirmed: bool) -> AppResult<RunOutcome>` — rejects ranges longer than **30 days** with `AppError::Validation` (spec §5.3 cap) and ALWAYS requires `confirmed == true` (returns `NeedsConfirmation` otherwise).

**Pipeline flow (spec §4 + §6), identical skeleton for both kinds:**
1. Load `view_assets` + `view_fields`; map to `GenAsset` / `GenField` / `FieldSpec`.
2. Estimate hits; `budget::today_hits`; `budget::check_level`. `HardConfirm` without `confirmed=true` → return `NeedsConfirmation` **before creating any run row**.
3. INSERT `run` row (`status='generating'`), create `pending/` dir, generate workbook, store `workbook_path` + `estimated_hits` on the run row.
4. `status='refreshing'` → `run_refresh`. On `AppError::Refresh { code: 2, .. }` (timeout): retry ONCE with `timeout × 2`. Any other/persisting failure → `status='failed'`, `error_summary`, workbook **stays in `pending/`**, return the error. `budget::record_hits` is called right after the (last) refresh attempt returns, success or failure — Bloomberg was hit either way.
5. `status='reading'` → `read_eod_workbook` / `read_backfill_workbook`. META mismatch or read error → `failed` (no ingest).
6. `status='ingesting'` → `ingest_outcome` (one transaction). Move workbook to `archive_path` (`std::fs::create_dir_all` + `rename`). Final status: `partial` if `summary.issues > 0` else `ok`; set `finished_at`.

- [ ] **Step 1: Write failing unit tests for path helpers**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use std::path::Path;

    #[test]
    fn pending_and_archive_paths_follow_spec_naming() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 13).unwrap();
        let p = pending_path(Path::new("C:\\bloomdata"), "core-eq", d);
        assert!(p.ends_with(Path::new("pending").join("core-eq_2026-08-13.xlsx")));
        let a = archive_path(Path::new("C:\\bloomdata"), 42, "core-eq", d);
        assert!(a.ends_with(Path::new("archive").join("2026").join("08")
            .join("run_42_core-eq_2026-08-13.xlsx")));
    }
}
```

- [ ] **Step 2: Run to verify failure** — compile FAIL.

- [ ] **Step 3: Implement `orchestrator.rs`**

```rust
use crate::budget::{self, BudgetLevel};
use crate::error::{AppError, AppResult};
use crate::excel_gen::{self, GenAsset, GenField, WbMeta};
use crate::excel_read::{self, FieldSpec};
use crate::ingest::{self, IngestSummary};
use crate::refresh_driver;
use crate::views;
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;
use std::path::{Path, PathBuf};

pub const BACKFILL_CAP_DAYS: i64 = 30;

#[derive(Debug, Clone)]
pub struct PipelineConfig {
    pub data_dir: PathBuf,
    pub script_path: PathBuf,
    pub refresh_timeout_s: u32,
    pub soft_limit: i64,
    pub dry_run_refresh: bool,
}

#[derive(Debug, Serialize)]
pub enum RunOutcome {
    Completed { run_id: i64, summary: IngestSummary },
    NeedsConfirmation { estimated: i64, today_total: i64 },
}

pub fn pending_path(data_dir: &Path, view_name: &str, date: NaiveDate) -> PathBuf {
    data_dir.join("pending").join(format!("{view_name}_{date}.xlsx"))
}

pub fn archive_path(data_dir: &Path, run_id: i64, view_name: &str, date: NaiveDate) -> PathBuf {
    data_dir.join("archive")
        .join(date.format("%Y").to_string())
        .join(date.format("%m").to_string())
        .join(format!("run_{run_id}_{view_name}_{date}.xlsx"))
}

struct Loaded {
    view_name: String,
    assets: Vec<GenAsset>,
    gen_fields: Vec<GenField>,
    field_specs: Vec<FieldSpec>,
}

async fn load_view(pool: &PgPool, view_id: i64) -> AppResult<Loaded> {
    let view = sqlx::query_as::<_, views::View>("SELECT * FROM view WHERE id = $1")
        .bind(view_id).fetch_one(pool).await?;
    let assets_db = views::view_assets(pool, view_id).await?;
    let fields_db = views::view_fields(pool, view_id).await?;
    let classes = crate::registry::list_asset_classes(pool).await?;
    let class_name = |id: i64| classes.iter().find(|c| c.id == id)
        .map(|c| c.name.clone()).unwrap_or_else(|| format!("Class{id}"));
    let assets = assets_db.iter().map(|a| GenAsset {
        asset_id: a.id, asset_class_id: a.asset_class_id,
        class_name: class_name(a.asset_class_id),
        label: a.label.clone(), bdp_security: a.bdp_security.clone(),
    }).collect();
    let gen_fields = fields_db.iter().map(|f| GenField {
        field_id: f.id, asset_class_id: f.asset_class_id, mnemonic: f.mnemonic.clone(),
    }).collect();
    let field_specs = fields_db.iter().map(|f| FieldSpec {
        field_id: f.id, asset_class_id: f.asset_class_id,
        mnemonic: f.mnemonic.clone(), value_kind: f.value_kind.clone(),
    }).collect();
    Ok(Loaded { view_name: view.name, assets, gen_fields, field_specs })
}

async fn set_status(pool: &PgPool, run_id: i64, status: &str) -> AppResult<()> {
    sqlx::query("UPDATE run SET status = $2 WHERE id = $1")
        .bind(run_id).bind(status).execute(pool).await?;
    Ok(())
}

async fn fail_run(pool: &PgPool, run_id: i64, err: &AppError) -> AppResult<()> {
    sqlx::query("UPDATE run SET status='failed', finished_at=now(), error_summary=$2 WHERE id=$1")
        .bind(run_id).bind(err.to_string()).execute(pool).await?;
    Ok(())
}

async fn refresh_with_retry(cfg: &PipelineConfig, wb: &Path) -> AppResult<()> {
    match refresh_driver::run_refresh(&cfg.script_path, wb,
                                      cfg.refresh_timeout_s, cfg.dry_run_refresh).await {
        Err(AppError::Refresh { code: 2, .. }) => {
            refresh_driver::run_refresh(&cfg.script_path, wb,
                                        cfg.refresh_timeout_s * 2, cfg.dry_run_refresh)
                .await.map(|_| ())
        }
        other => other.map(|_| ()),
    }
}

async fn finish(pool: &PgPool, cfg: &PipelineConfig, run_id: i64, view_name: &str,
                date: NaiveDate, wb: &Path, summary: IngestSummary) -> AppResult<RunOutcome> {
    let dest = archive_path(&cfg.data_dir, run_id, view_name, date);
    std::fs::create_dir_all(dest.parent().unwrap())?;
    std::fs::rename(wb, &dest)?;
    let status = if summary.issues > 0 { "partial" } else { "ok" };
    sqlx::query("UPDATE run SET status=$2, finished_at=now(), workbook_path=$3 WHERE id=$1")
        .bind(run_id).bind(status).bind(dest.to_string_lossy().as_ref())
        .execute(pool).await?;
    Ok(RunOutcome::Completed { run_id, summary })
}

pub async fn run_eod(pool: &PgPool, cfg: &PipelineConfig, view_id: i64,
                     trigger: &str, obs_date: NaiveDate, confirmed: bool)
    -> AppResult<RunOutcome> {
    let loaded = load_view(pool, view_id).await?;
    let estimated = budget::estimate_eod_hits(&loaded.assets, &loaded.field_specs);
    let today_total = budget::today_hits(pool).await?;
    if budget::check_level(estimated, today_total, cfg.soft_limit) == BudgetLevel::HardConfirm
        && !confirmed {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }

    let wb = pending_path(&cfg.data_dir, &loaded.view_name, obs_date);
    std::fs::create_dir_all(wb.parent().unwrap())?;
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, workbook_path, estimated_hits)
         VALUES ($1,'eod',$2,'generating',$3,$4) RETURNING id")
        .bind(view_id).bind(trigger)
        .bind(wb.to_string_lossy().as_ref()).bind(estimated)
        .fetch_one(pool).await?;

    let meta = WbMeta { run_id, view_id, kind: "eod".into(),
                        generated_at: chrono::Local::now().to_rfc3339() };
    if let Err(e) = excel_gen::generate_eod_workbook(&wb, &meta, &loaded.assets, &loaded.gen_fields) {
        fail_run(pool, run_id, &e).await?;
        return Err(e);
    }

    set_status(pool, run_id, "refreshing").await?;
    let refresh_result = refresh_with_retry(cfg, &wb).await;
    budget::record_hits(pool, run_id, estimated).await?;
    if let Err(e) = refresh_result {
        fail_run(pool, run_id, &e).await?;
        return Err(e);
    }

    set_status(pool, run_id, "reading").await?;
    let outcome = match excel_read::read_eod_workbook(
        &wb, run_id, &loaded.assets, &loaded.field_specs, obs_date) {
        Ok(o) => o,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };

    set_status(pool, run_id, "ingesting").await?;
    let summary = match ingest::ingest_outcome(pool, run_id, &outcome).await {
        Ok(s) => s,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };
    finish(pool, cfg, run_id, &loaded.view_name, obs_date, &wb, summary).await
}

pub async fn run_backfill(pool: &PgPool, cfg: &PipelineConfig, view_id: i64,
                          start: NaiveDate, end: NaiveDate, confirmed: bool)
    -> AppResult<RunOutcome> {
    if start > end {
        return Err(AppError::Validation("start after end".into()));
    }
    if (end - start).num_days() + 1 > BACKFILL_CAP_DAYS {
        return Err(AppError::Validation(
            format!("backfill range exceeds {BACKFILL_CAP_DAYS}-day cap")));
    }
    let loaded = load_view(pool, view_id).await?;
    let estimated = budget::estimate_backfill_hits(&loaded.assets, &loaded.field_specs, start, end);
    let today_total = budget::today_hits(pool).await?;
    // Spec §5.3: every backfill shows its cost and requires explicit confirmation.
    if !confirmed {
        return Ok(RunOutcome::NeedsConfirmation { estimated, today_total });
    }

    let wb = pending_path(&cfg.data_dir, &loaded.view_name, end);
    std::fs::create_dir_all(wb.parent().unwrap())?;
    let run_id: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status, workbook_path, estimated_hits)
         VALUES ($1,'backfill','manual','generating',$2,$3) RETURNING id")
        .bind(view_id).bind(wb.to_string_lossy().as_ref()).bind(estimated)
        .fetch_one(pool).await?;

    let meta = WbMeta { run_id, view_id, kind: "backfill".into(),
                        generated_at: chrono::Local::now().to_rfc3339() };
    if let Err(e) = excel_gen::generate_backfill_workbook(
        &wb, &meta, &loaded.assets, &loaded.gen_fields, start, end) {
        fail_run(pool, run_id, &e).await?;
        return Err(e);
    }

    set_status(pool, run_id, "refreshing").await?;
    let refresh_result = refresh_with_retry(cfg, &wb).await;
    budget::record_hits(pool, run_id, estimated).await?;
    if let Err(e) = refresh_result {
        fail_run(pool, run_id, &e).await?;
        return Err(e);
    }

    set_status(pool, run_id, "reading").await?;
    let outcome = match excel_read::read_backfill_workbook(
        &wb, run_id, &loaded.assets, &loaded.field_specs) {
        Ok(o) => o,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };

    set_status(pool, run_id, "ingesting").await?;
    let summary = match ingest::ingest_outcome(pool, run_id, &outcome).await {
        Ok(s) => s,
        Err(e) => { fail_run(pool, run_id, &e).await?; return Err(e); }
    };
    finish(pool, cfg, run_id, &loaded.view_name, end, &wb, summary).await
}
```

- [ ] **Step 4: Declare the `DataFetcher` trait (spec §2.4)** — append to `orchestrator.rs`:

```rust
/// Seam for swapping the Excel/COM fetch path for direct BLPAPI later (spec §2.4).
/// The trait covers stages 1–3 (generate → refresh → read); ingest is fetcher-agnostic.
pub trait DataFetcher {
    fn fetch_eod(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                 gen_fields: &[GenField], field_specs: &[FieldSpec],
                 obs_date: NaiveDate)
        -> impl std::future::Future<Output = AppResult<excel_read::ReadOutcome>> + Send;
    fn fetch_history(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                     gen_fields: &[GenField], field_specs: &[FieldSpec],
                     start: NaiveDate, end: NaiveDate)
        -> impl std::future::Future<Output = AppResult<excel_read::ReadOutcome>> + Send;
}

pub struct ExcelComFetcher<'a> { pub cfg: &'a PipelineConfig }

impl DataFetcher for ExcelComFetcher<'_> {
    async fn fetch_eod(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                       gen_fields: &[GenField], field_specs: &[FieldSpec],
                       obs_date: NaiveDate) -> AppResult<excel_read::ReadOutcome> {
        excel_gen::generate_eod_workbook(wb, meta, assets, gen_fields)?;
        refresh_with_retry(self.cfg, wb).await?;
        excel_read::read_eod_workbook(wb, meta.run_id, assets, field_specs, obs_date)
    }
    async fn fetch_history(&self, wb: &Path, meta: &WbMeta, assets: &[GenAsset],
                           gen_fields: &[GenField], field_specs: &[FieldSpec],
                           start: NaiveDate, end: NaiveDate)
        -> AppResult<excel_read::ReadOutcome> {
        excel_gen::generate_backfill_workbook(wb, meta, assets, gen_fields, start, end)?;
        refresh_with_retry(self.cfg, wb).await?;
        excel_read::read_backfill_workbook(wb, meta.run_id, assets, field_specs)
    }
}
```

Then refactor `run_eod`/`run_backfill` from Step 3 to call `ExcelComFetcher { cfg }.fetch_eod(...)` / `.fetch_history(...)` between the status updates instead of calling the three stage functions inline — the status transitions (`generating` → `refreshing` → `reading`), `budget::record_hits` after the refresh attempt, and the `fail_run` error handling stay in `run_eod`/`run_backfill` exactly as written (record hits when the fetcher returns, success or `AppError::Refresh` failure). A future `BlpapiFetcher` implements the same trait without touching the orchestrator flow.

- [ ] **Step 5: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml orchestrator` → PASS; whole suite green.

- [ ] **Step 6: Add ignored end-to-end dry-run test** (append to `tests/db_integration.rs`)

Requires postgres AND Excel (no Bloomberg): with `dry_run_refresh: true` the driver opens/saves without refreshing, so BDP cells come back as errors and the run must end `partial` with issues — which proves generate → COM → read → ingest wiring end to end.

```rust
#[tokio::test]
#[ignore = "requires postgres and excel"]
async fn eod_pipeline_dry_run_ends_partial() {
    use getbloomdata_lib::{db, fields, orchestrator, registry, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let class = registry::create_asset_class(&pool, "EquityT11", "t").await.unwrap();
    fields::create_field(&pool, class.id, "PX_LAST_T11", "px", "numeric").await.unwrap();
    let a = registry::create_asset(&pool, registry::NewAsset {
        asset_class_id: class.id, label: "T11".into(), id_kind: "ticker".into(),
        ticker: Some("AAPL US".into()), isin: None, yellow_key: "Equity".into(),
    }).await.unwrap();
    let v = views::create_view(&pool, "t11-view", "").await.unwrap();
    views::set_view_assets(&pool, v.id, &[a.id]).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let cfg = orchestrator::PipelineConfig {
        data_dir: dir.path().to_path_buf(),
        script_path: std::path::PathBuf::from("scripts/refresh.ps1"),
        refresh_timeout_s: 60,
        soft_limit: 100_000,
        dry_run_refresh: true,
    };
    let d = chrono::Local::now().date_naive();
    let out = orchestrator::run_eod(&pool, &cfg, v.id, "manual", d, false).await.unwrap();
    match out {
        orchestrator::RunOutcome::Completed { run_id, summary } => {
            assert!(summary.issues > 0);  // BDP can't evaluate without the add-in
            let status: String = sqlx::query_scalar("SELECT status FROM run WHERE id=$1")
                .bind(run_id).fetch_one(&pool).await.unwrap();
            assert_eq!(status, "partial");
        }
        other => panic!("expected Completed, got {other:?}"),
    }
}
```

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src src-tauri/tests
git commit -m "feat: 4-stage pipeline orchestrator with DataFetcher seam, retry, budget gate, and archiving"
```

---

### Task 12: Scheduler — random draw, catch-up, gap detection (`scheduler.rs`)

**Files:**
- Create: `src-tauri/src/scheduler.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod scheduler;`)
- Test: unit tests in-module (seeded RNG + synthetic dates; no DB); ignored DB test for draw persistence

**Interfaces:**
- Consumes: `orchestrator::{run_eod, PipelineConfig}`, `AppResult`.
- Produces:
  - `pub fn draw_time(window_start: NaiveTime, window_end: NaiveTime, rng: &mut impl rand::Rng) -> NaiveTime` — uniform at second granularity in `[start, end)`.
  - `pub async fn ensure_draw(pool: &PgPool, schedule_id: i64, today: NaiveDate) -> AppResult<NaiveTime>` — returns existing `drawn_at` if `drawn_for == today`, else draws with `rand::thread_rng()`, persists both columns, returns it. **Never re-rolls within a day** (spec §5.1).
  - `pub async fn already_ran_today(pool: &PgPool, view_id: i64, today: NaiveDate) -> AppResult<bool>` — true if a `run` row (`kind='eod'`, `trigger_kind='scheduled'`, any non-failed status) exists for that view with `started_at::date = today`.
  - `pub fn is_due(now: NaiveTime, drawn_at: NaiveTime) -> bool` — `now >= drawn_at` (covers catch-up: a late app launch is still "due" for today).
  - `pub async fn tick(pool: &PgPool, cfg: &PipelineConfig, now: chrono::DateTime<chrono::Local>) -> AppResult<Vec<i64>>` — for every active schedule: `ensure_draw`; if due and not already run, call `run_eod(trigger="scheduled", confirmed=false)`; a `NeedsConfirmation` or error is written to `schedule.last_result` and skipped (never blocks other schedules). Returns view_ids launched. Task 13 wires this into a 60-second `tokio::time::interval` loop.
  - `pub fn missing_weekdays(present: &std::collections::HashSet<NaiveDate>, start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate>`
  - `pub fn group_ranges(dates: &[NaiveDate], cap_days: i64) -> Vec<(NaiveDate, NaiveDate)>` — contiguous (by weekday succession) ranges, each spanning ≤ cap_days calendar days.
  - `pub async fn detect_gaps(pool: &PgPool, view_id: i64, lookback_days: i64, today: NaiveDate) -> AppResult<Vec<(NaiveDate, NaiveDate)>>` — weekdays in `[today − lookback_days, yesterday]` with zero observations for the view's assets, grouped with `cap_days = 30`. (Spec §5.2: BDH later returns only trading days, so holidays simply never fill — the DB converges on Bloomberg's calendar.)

- [ ] **Step 1: Write failing unit tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{NaiveDate, NaiveTime};
    use rand::rngs::StdRng;
    use rand::SeedableRng;
    use std::collections::HashSet;

    #[test]
    fn draw_stays_inside_window_and_varies() {
        let s = NaiveTime::from_hms_opt(9, 0, 0).unwrap();
        let e = NaiveTime::from_hms_opt(18, 0, 0).unwrap();
        let mut rng = StdRng::seed_from_u64(42);
        let mut seen = HashSet::new();
        for _ in 0..200 {
            let t = draw_time(s, e, &mut rng);
            assert!(t >= s && t < e, "drew {t} outside window");
            seen.insert(t);
        }
        assert!(seen.len() > 150, "draws should vary, got {} distinct", seen.len());
    }

    #[test]
    fn due_logic_covers_catchup() {
        let drawn = NaiveTime::from_hms_opt(11, 30, 0).unwrap();
        assert!(!is_due(NaiveTime::from_hms_opt(9, 0, 0).unwrap(), drawn));
        assert!(is_due(NaiveTime::from_hms_opt(11, 30, 0).unwrap(), drawn));
        assert!(is_due(NaiveTime::from_hms_opt(17, 59, 0).unwrap(), drawn)); // late launch
    }

    #[test]
    fn missing_weekdays_ignores_weekends() {
        let mut present = HashSet::new();
        present.insert(NaiveDate::from_ymd_opt(2026, 8, 10).unwrap()); // Mon
        present.insert(NaiveDate::from_ymd_opt(2026, 8, 12).unwrap()); // Wed
        let start = NaiveDate::from_ymd_opt(2026, 8, 8).unwrap();  // Sat
        let end = NaiveDate::from_ymd_opt(2026, 8, 12).unwrap();   // Wed
        let missing = missing_weekdays(&present, start, end);
        assert_eq!(missing, vec![NaiveDate::from_ymd_opt(2026, 8, 11).unwrap()]); // Tue only
    }

    #[test]
    fn ranges_group_contiguous_weekdays_and_respect_cap() {
        let d = |m: u32, day: u32| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        // Thu 8/6, Fri 8/7, Mon 8/10 are weekday-contiguous; Wed 8/19 is separate
        let ranges = group_ranges(&[d(8,6), d(8,7), d(8,10), d(8,19)], 30);
        assert_eq!(ranges, vec![(d(8,6), d(8,10)), (d(8,19), d(8,19))]);
        // cap splits long runs
        let long: Vec<_> = (0..40)
            .map(|i| d(6, 1) + chrono::Duration::days(i))
            .filter(|x| !matches!(x.weekday(),
                chrono::Weekday::Sat | chrono::Weekday::Sun))
            .collect();
        for (s, e) in group_ranges(&long, 30) {
            assert!((e - s).num_days() < 30);
        }
    }
}
```

- [ ] **Step 2: Run to verify failure** — compile FAIL.

- [ ] **Step 3: Implement `scheduler.rs`**

```rust
use crate::error::AppResult;
use crate::orchestrator::{self, PipelineConfig, RunOutcome};
use chrono::{Datelike, Duration, NaiveDate, NaiveTime, Weekday};
use rand::Rng;
use sqlx::PgPool;
use std::collections::HashSet;

pub fn draw_time(window_start: NaiveTime, window_end: NaiveTime,
                 rng: &mut impl Rng) -> NaiveTime {
    let start_s = window_start.signed_duration_since(
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()).num_seconds();
    let end_s = window_end.signed_duration_since(
        NaiveTime::from_hms_opt(0, 0, 0).unwrap()).num_seconds();
    let s = rng.gen_range(start_s..end_s);
    NaiveTime::from_num_seconds_from_midnight_opt(s as u32, 0).unwrap()
}

pub fn is_due(now: NaiveTime, drawn_at: NaiveTime) -> bool {
    now >= drawn_at
}

pub async fn ensure_draw(pool: &PgPool, schedule_id: i64, today: NaiveDate)
    -> AppResult<NaiveTime> {
    let row: (Option<NaiveDate>, Option<NaiveTime>, NaiveTime, NaiveTime) =
        sqlx::query_as(
            "SELECT drawn_for, drawn_at, window_start, window_end
             FROM schedule WHERE id = $1")
        .bind(schedule_id).fetch_one(pool).await?;
    if let (Some(df), Some(da)) = (row.0, row.1) {
        if df == today {
            return Ok(da);  // never re-roll within a day
        }
    }
    let t = draw_time(row.2, row.3, &mut rand::thread_rng());
    sqlx::query("UPDATE schedule SET drawn_for = $2, drawn_at = $3 WHERE id = $1")
        .bind(schedule_id).bind(today).bind(t).execute(pool).await?;
    Ok(t)
}

pub async fn already_ran_today(pool: &PgPool, view_id: i64, today: NaiveDate)
    -> AppResult<bool> {
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM run
         WHERE view_id = $1 AND kind = 'eod' AND trigger_kind = 'scheduled'
           AND status <> 'failed' AND started_at::date = $2")
        .bind(view_id).bind(today).fetch_one(pool).await?;
    Ok(n > 0)
}

pub async fn tick(pool: &PgPool, cfg: &PipelineConfig,
                  now: chrono::DateTime<chrono::Local>) -> AppResult<Vec<i64>> {
    let today = now.date_naive();
    let schedules: Vec<(i64, i64)> = sqlx::query_as(
        "SELECT id, view_id FROM schedule WHERE active")
        .fetch_all(pool).await?;
    let mut launched = Vec::new();
    for (sid, view_id) in schedules {
        let drawn = ensure_draw(pool, sid, today).await?;
        if !is_due(now.time(), drawn) || already_ran_today(pool, view_id, today).await? {
            continue;
        }
        let result = orchestrator::run_eod(pool, cfg, view_id, "scheduled", today, false).await;
        let msg = match &result {
            Ok(RunOutcome::Completed { run_id, summary }) =>
                format!("ok run={run_id} upserted={} issues={}",
                        summary.upserted, summary.issues),
            Ok(RunOutcome::NeedsConfirmation { estimated, .. }) =>
                format!("blocked: needs confirmation for {estimated} estimated hits"),
            Err(e) => format!("failed: {e}"),
        };
        sqlx::query("UPDATE schedule SET last_result = $2 WHERE id = $1")
            .bind(sid).bind(&msg).execute(pool).await?;
        if matches!(result, Ok(RunOutcome::Completed { .. })) {
            launched.push(view_id);
        }
    }
    Ok(launched)
}

pub fn missing_weekdays(present: &HashSet<NaiveDate>,
                        start: NaiveDate, end: NaiveDate) -> Vec<NaiveDate> {
    let mut out = Vec::new();
    let mut d = start;
    while d <= end {
        if !matches!(d.weekday(), Weekday::Sat | Weekday::Sun) && !present.contains(&d) {
            out.push(d);
        }
        d += Duration::days(1);
    }
    out
}

fn next_weekday(d: NaiveDate) -> NaiveDate {
    let mut n = d + Duration::days(1);
    while matches!(n.weekday(), Weekday::Sat | Weekday::Sun) {
        n += Duration::days(1);
    }
    n
}

pub fn group_ranges(dates: &[NaiveDate], cap_days: i64) -> Vec<(NaiveDate, NaiveDate)> {
    let mut out: Vec<(NaiveDate, NaiveDate)> = Vec::new();
    for &d in dates {
        match out.last_mut() {
            Some((s, e)) if next_weekday(*e) == d && (d - *s).num_days() < cap_days =>
                *e = d,
            _ => out.push((d, d)),
        }
    }
    out
}

pub async fn detect_gaps(pool: &PgPool, view_id: i64, lookback_days: i64,
                         today: NaiveDate) -> AppResult<Vec<(NaiveDate, NaiveDate)>> {
    let start = today - Duration::days(lookback_days);
    let end = today - Duration::days(1);
    let rows: Vec<(NaiveDate,)> = sqlx::query_as(
        "SELECT DISTINCT o.obs_date FROM observation o
         JOIN view_asset va ON va.asset_id = o.asset_id
         WHERE va.view_id = $1 AND o.obs_date BETWEEN $2 AND $3")
        .bind(view_id).bind(start).bind(end).fetch_all(pool).await?;
    let present: HashSet<NaiveDate> = rows.into_iter().map(|r| r.0).collect();
    Ok(group_ranges(&missing_weekdays(&present, start, end),
                    orchestrator::BACKFILL_CAP_DAYS))
}
```

- [ ] **Step 4: Run tests** — `cargo test --manifest-path src-tauri/Cargo.toml scheduler` → PASS.

- [ ] **Step 5: Add ignored DB test — draw persists, no re-roll** (append to `tests/db_integration.rs`)

```rust
#[tokio::test]
#[ignore = "requires postgres"]
async fn schedule_draw_persists_within_day() {
    use getbloomdata_lib::{db, scheduler, views};
    let url = test_url().expect("set BLOOM_TEST_DATABASE_URL");
    let pool = db::connect(&url).await.unwrap();
    let v = views::create_view(&pool, "t12-view", "").await.unwrap();
    let sid: i64 = sqlx::query_scalar(
        "INSERT INTO schedule (view_id) VALUES ($1) RETURNING id")
        .bind(v.id).fetch_one(&pool).await.unwrap();
    let today = chrono::Local::now().date_naive();
    let first = scheduler::ensure_draw(&pool, sid, today).await.unwrap();
    let second = scheduler::ensure_draw(&pool, sid, today).await.unwrap();
    assert_eq!(first, second);  // restart must not re-roll
    let win_s = chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap();
    let win_e = chrono::NaiveTime::from_hms_opt(18, 0, 0).unwrap();
    assert!(first >= win_s && first < win_e);
}
```

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src src-tauri/tests
git commit -m "feat: randomized-window scheduler with persistent draw and weekday gap detection"
```

---

### Task 13: Tauri IPC layer and app wiring (`commands.rs`, `lib.rs`, `main.rs`)

**Files:**
- Create: `src-tauri/src/commands.rs`
- Modify: `src-tauri/src/lib.rs` (AppState, run(), scheduler loop, module declarations)
- Test: `cargo test` stays green; `npm run tauri dev` manual check

**Interfaces:**
- Consumes: everything from Tasks 2–12.
- Produces (frontend contract — `src/lib/api.ts` in Task 14 mirrors these exactly):
  - `pub struct AppState { pub pool: PgPool, pub cfg: tokio::sync::RwLock<AppConfig> }`
  - `pub struct AppConfig { pub data_dir: String, pub soft_limit: i64, pub refresh_timeout_s: u32 }` (derives `Serialize, Deserialize, Clone`) — persisted as JSON at `<data_dir default: C:\bloomdata>\config.json`; `database_url` comes from env `BLOOM_DATABASE_URL` (default `postgres://postgres:postgres@localhost/bloomdata`), never stored in the JSON.
  - Commands (all `#[tauri::command] async fn ... -> Result<T, AppError>`):
    `list_asset_classes() -> Vec<AssetClass>` · `create_asset_class(name, description) -> AssetClass` · `list_assets() -> Vec<Asset>` · `create_asset(new: NewAsset) -> Asset` · `set_asset_active(asset_id, active)` · `list_fields() -> Vec<FieldDef>` · `create_field(asset_class_id, mnemonic, label, value_kind) -> FieldDef` · `list_views() -> Vec<View>` · `create_view(name, description) -> View` · `set_view_assets(view_id, asset_ids: Vec<i64>)` · `set_view_fields(view_id, field_ids: Vec<i64>)` · `get_view_assets(view_id) -> Vec<Asset>` · `get_view_fields(view_id) -> Vec<FieldDef>` · `estimate_view(view_id) -> EstimateOut` · `run_eod_now(view_id, confirmed: bool) -> RunOutcome` · `run_backfill_now(view_id, start: String, end: String, confirmed: bool) -> RunOutcome` (ISO dates) · `list_runs(limit: i64) -> Vec<RunRow>` · `list_issues(run_id) -> Vec<IssueRow>` · `detect_view_gaps(view_id) -> Vec<(String, String)>` · `list_schedules() -> Vec<ScheduleRow>` · `upsert_schedule(view_id, window_start: String, window_end: String, active: bool)` · `get_settings() -> AppConfig` · `save_settings(cfg: AppConfig)`
  - `pub struct EstimateOut { pub estimated: i64, pub today_total: i64, pub level: BudgetLevel }`
  - `pub struct RunRow`, `pub struct IssueRow`, `pub struct ScheduleRow` — `sqlx::FromRow + Serialize` mirrors of the `run`, `ingest_issue`, `schedule` tables (same column names/types as the migration).

- [ ] **Step 1: Implement `commands.rs`**

Every command follows the same 3-line shape — the module is plumbing, not logic. Representative implementations (repeat the pattern for every command listed above; each is a direct call into the Task 3–12 function of the same name):

```rust
use crate::error::{AppError, AppResult};
use crate::orchestrator::{self, PipelineConfig, RunOutcome};
use crate::{budget, fields, registry, scheduler, views};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::path::PathBuf;
use tauri::State;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub data_dir: String,
    pub soft_limit: i64,
    pub refresh_timeout_s: u32,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self { data_dir: "C:\\bloomdata".into(),
               soft_limit: budget::DEFAULT_SOFT_LIMIT,
               refresh_timeout_s: 600 }
    }
}

pub struct AppState {
    pub pool: PgPool,
    pub cfg: tokio::sync::RwLock<AppConfig>,
}

pub fn script_path() -> PathBuf {
    // scripts/refresh.ps1 ships next to the executable (bundled as a Tauri resource);
    // in dev it resolves relative to src-tauri/.
    let exe_dir = std::env::current_exe().ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let bundled = exe_dir.join("scripts").join("refresh.ps1");
    if bundled.exists() { bundled } else { PathBuf::from("scripts/refresh.ps1") }
}

pub async fn pipeline_cfg(state: &AppState) -> PipelineConfig {
    let c = state.cfg.read().await.clone();
    PipelineConfig {
        data_dir: PathBuf::from(c.data_dir),
        script_path: script_path(),
        refresh_timeout_s: c.refresh_timeout_s,
        soft_limit: c.soft_limit,
        dry_run_refresh: false,
    }
}

#[derive(Debug, Serialize)]
pub struct EstimateOut {
    pub estimated: i64,
    pub today_total: i64,
    pub level: budget::BudgetLevel,
}

#[tauri::command]
pub async fn list_assets(state: State<'_, AppState>) -> Result<Vec<registry::Asset>, AppError> {
    registry::list_assets(&state.pool).await
}

#[tauri::command]
pub async fn create_asset(state: State<'_, AppState>, new: registry::NewAsset)
    -> Result<registry::Asset, AppError> {
    registry::create_asset(&state.pool, new).await
}

#[tauri::command]
pub async fn estimate_view(state: State<'_, AppState>, view_id: i64)
    -> Result<EstimateOut, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let assets = views::view_assets(&state.pool, view_id).await?;
    let fields_db = views::view_fields(&state.pool, view_id).await?;
    let gen: Vec<_> = assets.iter().map(|a| crate::excel_gen::GenAsset {
        asset_id: a.id, asset_class_id: a.asset_class_id,
        class_name: String::new(), label: a.label.clone(),
        bdp_security: a.bdp_security.clone() }).collect();
    let specs: Vec<_> = fields_db.iter().map(|f| crate::excel_read::FieldSpec {
        field_id: f.id, asset_class_id: f.asset_class_id,
        mnemonic: f.mnemonic.clone(), value_kind: f.value_kind.clone() }).collect();
    let estimated = budget::estimate_eod_hits(&gen, &specs);
    let today_total = budget::today_hits(&state.pool).await?;
    let level = budget::check_level(estimated, today_total, cfg.soft_limit);
    Ok(EstimateOut { estimated, today_total, level })
}

#[tauri::command]
pub async fn run_eod_now(state: State<'_, AppState>, view_id: i64, confirmed: bool)
    -> Result<RunOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let today = chrono::Local::now().date_naive();
    orchestrator::run_eod(&state.pool, &cfg, view_id, "manual", today, confirmed).await
}

#[tauri::command]
pub async fn run_backfill_now(state: State<'_, AppState>, view_id: i64,
                              start: String, end: String, confirmed: bool)
    -> Result<RunOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let s = start.parse().map_err(|_| AppError::Validation("bad start date".into()))?;
    let e = end.parse().map_err(|_| AppError::Validation("bad end date".into()))?;
    orchestrator::run_backfill(&state.pool, &cfg, view_id, s, e, confirmed).await
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct RunRow {
    pub id: i64, pub view_id: i64, pub kind: String, pub trigger_kind: String,
    pub status: String, pub started_at: chrono::DateTime<chrono::Utc>,
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    pub estimated_hits: i64, pub error_summary: Option<String>,
}

#[tauri::command]
pub async fn list_runs(state: State<'_, AppState>, limit: i64)
    -> Result<Vec<RunRow>, AppError> {
    Ok(sqlx::query_as::<_, RunRow>(
        "SELECT id, view_id, kind, trigger_kind, status, started_at,
                finished_at, estimated_hits, error_summary
         FROM run ORDER BY id DESC LIMIT $1")
        .bind(limit).fetch_all(&state.pool).await?)
}

#[tauri::command]
pub async fn detect_view_gaps(state: State<'_, AppState>, view_id: i64)
    -> Result<Vec<(String, String)>, AppError> {
    let today = chrono::Local::now().date_naive();
    Ok(scheduler::detect_gaps(&state.pool, view_id, 30, today).await?
        .into_iter().map(|(s, e)| (s.to_string(), e.to_string())).collect())
}

#[tauri::command]
pub async fn get_settings(state: State<'_, AppState>) -> Result<AppConfig, AppError> {
    Ok(state.cfg.read().await.clone())
}

#[tauri::command]
pub async fn save_settings(state: State<'_, AppState>, cfg: AppConfig)
    -> Result<(), AppError> {
    let path = PathBuf::from(&cfg.data_dir).join("config.json");
    std::fs::create_dir_all(&cfg.data_dir)?;
    std::fs::write(&path, serde_json::to_string_pretty(&cfg)
        .map_err(|e| AppError::Validation(e.to_string()))?)?;
    *state.cfg.write().await = cfg;
    Ok(())
}
```

Write the remaining commands (`list_asset_classes`, `create_asset_class`, `set_asset_active`, `list_fields`, `create_field`, `list_views`, `create_view`, `set_view_assets`, `set_view_fields`, `get_view_assets`, `get_view_fields`, `list_issues`, `list_schedules`, `upsert_schedule`) in the same delegate style — each binds `state.pool` and calls the module function or a one-line `sqlx::query_as`. For `upsert_schedule`, first create `src-tauri/migrations/0002_schedule_unique.sql` containing exactly `ALTER TABLE schedule ADD CONSTRAINT schedule_view_unique UNIQUE (view_id);`, then implement the command as:

```rust
#[tauri::command]
pub async fn upsert_schedule(state: State<'_, AppState>, view_id: i64,
                             window_start: String, window_end: String, active: bool)
    -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO schedule (view_id, window_start, window_end, active)
         VALUES ($1, $2::time, $3::time, $4)
         ON CONFLICT (view_id) DO UPDATE
           SET window_start = EXCLUDED.window_start,
               window_end = EXCLUDED.window_end,
               active = EXCLUDED.active,
               drawn_for = NULL, drawn_at = NULL")  // window changed: force a fresh draw
        .bind(view_id).bind(window_start).bind(window_end).bind(active)
        .execute(&state.pool).await?;
    Ok(())
}
```

- [ ] **Step 2: Wire `lib.rs` run()**

```rust
pub mod budget; pub mod commands; pub mod db; pub mod error; pub mod excel_gen;
pub mod excel_read; pub mod fields; pub mod ingest; pub mod orchestrator;
pub mod refresh_driver; pub mod registry; pub mod scheduler; pub mod views;

use commands::{AppConfig, AppState};

fn load_config() -> AppConfig {
    let default = AppConfig::default();
    let path = std::path::PathBuf::from(&default.data_dir).join("config.json");
    std::fs::read_to_string(path).ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(default)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
    let cfg = load_config();
    let url = std::env::var("BLOOM_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/bloomdata".into());
    let pool = rt.block_on(db::connect(&url)).expect("database connection + migrations");

    let state = AppState { pool: pool.clone(), cfg: tokio::sync::RwLock::new(cfg.clone()) };

    // scheduler heartbeat: every 60 s, fire any schedule whose drawn time has passed
    rt.spawn({
        let pool = pool.clone();
        async move {
            let mut iv = tokio::time::interval(std::time::Duration::from_secs(60));
            loop {
                iv.tick().await;
                let cfg = commands::AppConfig::default(); // re-read persisted config each tick
                let pc = orchestrator::PipelineConfig {
                    data_dir: std::path::PathBuf::from(&cfg.data_dir),
                    script_path: commands::script_path(),
                    refresh_timeout_s: cfg.refresh_timeout_s,
                    soft_limit: cfg.soft_limit,
                    dry_run_refresh: false,
                };
                if let Err(e) = scheduler::tick(&pool, &pc, chrono::Local::now()).await {
                    eprintln!("scheduler tick failed: {e}");
                }
            }
        }
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            commands::list_asset_classes, commands::create_asset_class,
            commands::list_assets, commands::create_asset, commands::set_asset_active,
            commands::list_fields, commands::create_field,
            commands::list_views, commands::create_view,
            commands::set_view_assets, commands::set_view_fields,
            commands::get_view_assets, commands::get_view_fields,
            commands::estimate_view, commands::run_eod_now, commands::run_backfill_now,
            commands::list_runs, commands::list_issues, commands::detect_view_gaps,
            commands::list_schedules, commands::upsert_schedule,
            commands::get_settings, commands::save_settings,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

(Refinement while implementing: replace the scheduler loop's `AppConfig::default()` with the `load_config()` call so settings edits apply — the line above shows placement, `load_config()` is the correct call.) Also add `"resources": ["scripts/refresh.ps1"]` under `bundle` in `tauri.conf.json` so the script ships with the app.

- [ ] **Step 3: Verify** — `cargo test --manifest-path src-tauri/Cargo.toml` green; `npm run tauri dev` starts (requires local postgres with `bloomdata` DB created: `createdb bloomdata` or via pgAdmin; TimescaleDB extension installed).

- [ ] **Step 4: Commit**

```bash
git add src-tauri
git commit -m "feat: tauri commands, app state, config persistence, scheduler heartbeat"
```

---

### Task 14: Frontend — api wrapper and four screens (Svelte 5)

**Files:**
- Create: `src/lib/api.ts`, `src/lib/AssetsScreen.svelte`, `src/lib/ViewsScreen.svelte`, `src/lib/RunScreen.svelte`, `src/lib/SettingsScreen.svelte`
- Modify: `src/App.svelte` (replace template content with the tab shell)
- Test: `npm run check` (svelte-check) + manual walk-through in `npm run tauri dev`

**Interfaces:**
- Consumes: the Task 13 command names and payload shapes, verbatim — a rename on either side breaks the IPC contract.
- Produces: a usable UI. No component keeps its own copy of server state beyond the screen it renders; every mutation re-fetches its list.

- [ ] **Step 1: Write `src/lib/api.ts`** — the full IPC surface, typed:

```typescript
import { invoke } from "@tauri-apps/api/core";

export interface AssetClass { id: number; name: string; description: string; }
export interface Asset {
  id: number; asset_class_id: number; label: string; id_kind: string;
  ticker: string | null; isin: string | null; yellow_key: string;
  bdp_security: string; active: boolean;
}
export interface NewAsset {
  asset_class_id: number; label: string; id_kind: string;
  ticker: string | null; isin: string | null; yellow_key: string;
}
export interface FieldDef {
  id: number; asset_class_id: number; mnemonic: string;
  label: string; value_kind: string; active: boolean;
}
export interface View { id: number; name: string; description: string; active: boolean; }
export interface EstimateOut {
  estimated: number; today_total: number; level: "Ok" | "SoftWarn" | "HardConfirm";
}
export type RunOutcome =
  | { Completed: { run_id: number; summary: { upserted: number; issues: number } } }
  | { NeedsConfirmation: { estimated: number; today_total: number } };
export interface RunRow {
  id: number; view_id: number; kind: string; trigger_kind: string; status: string;
  started_at: string; finished_at: string | null;
  estimated_hits: number; error_summary: string | null;
}
export interface IssueRow {
  id: number; run_id: number; asset_id: number | null; field_id: number | null;
  obs_date: string | null; severity: string; code: string; detail: string;
}
export interface ScheduleRow {
  id: number; view_id: number; active: boolean; window_start: string;
  window_end: string; drawn_for: string | null; drawn_at: string | null;
  last_result: string | null;
}
export interface AppConfig { data_dir: string; soft_limit: number; refresh_timeout_s: number; }

export const api = {
  listAssetClasses: () => invoke<AssetClass[]>("list_asset_classes"),
  createAssetClass: (name: string, description: string) =>
    invoke<AssetClass>("create_asset_class", { name, description }),
  listAssets: () => invoke<Asset[]>("list_assets"),
  createAsset: (newAsset: NewAsset) => invoke<Asset>("create_asset", { new: newAsset }),
  setAssetActive: (assetId: number, active: boolean) =>
    invoke<void>("set_asset_active", { assetId, active }),
  listFields: () => invoke<FieldDef[]>("list_fields"),
  createField: (assetClassId: number, mnemonic: string, label: string, valueKind: string) =>
    invoke<FieldDef>("create_field", { assetClassId, mnemonic, label, valueKind }),
  listViews: () => invoke<View[]>("list_views"),
  createView: (name: string, description: string) =>
    invoke<View>("create_view", { name, description }),
  setViewAssets: (viewId: number, assetIds: number[]) =>
    invoke<void>("set_view_assets", { viewId, assetIds }),
  setViewFields: (viewId: number, fieldIds: number[]) =>
    invoke<void>("set_view_fields", { viewId, fieldIds }),
  getViewAssets: (viewId: number) => invoke<Asset[]>("get_view_assets", { viewId }),
  getViewFields: (viewId: number) => invoke<FieldDef[]>("get_view_fields", { viewId }),
  estimateView: (viewId: number) => invoke<EstimateOut>("estimate_view", { viewId }),
  runEodNow: (viewId: number, confirmed: boolean) =>
    invoke<RunOutcome>("run_eod_now", { viewId, confirmed }),
  runBackfillNow: (viewId: number, start: string, end: string, confirmed: boolean) =>
    invoke<RunOutcome>("run_backfill_now", { viewId, start, end, confirmed }),
  listRuns: (limit: number) => invoke<RunRow[]>("list_runs", { limit }),
  listIssues: (runId: number) => invoke<IssueRow[]>("list_issues", { runId }),
  detectViewGaps: (viewId: number) => invoke<[string, string][]>("detect_view_gaps", { viewId }),
  listSchedules: () => invoke<ScheduleRow[]>("list_schedules"),
  upsertSchedule: (viewId: number, windowStart: string, windowEnd: string, active: boolean) =>
    invoke<void>("upsert_schedule", { viewId, windowStart, windowEnd, active }),
  getSettings: () => invoke<AppConfig>("get_settings"),
  saveSettings: (cfg: AppConfig) => invoke<void>("save_settings", { cfg }),
};
```

- [ ] **Step 2: Write `src/App.svelte`** — tab shell:

```svelte
<script lang="ts">
  import AssetsScreen from "./lib/AssetsScreen.svelte";
  import ViewsScreen from "./lib/ViewsScreen.svelte";
  import RunScreen from "./lib/RunScreen.svelte";
  import SettingsScreen from "./lib/SettingsScreen.svelte";
  let tab = $state<"assets" | "views" | "run" | "settings">("run");
</script>

<main>
  <nav>
    {#each [["run","Run"],["assets","Assets"],["views","Views"],["settings","Settings"]] as [id, label]}
      <button class:active={tab === id} onclick={() => (tab = id as typeof tab)}>{label}</button>
    {/each}
  </nav>
  {#if tab === "assets"}<AssetsScreen />{:else if tab === "views"}<ViewsScreen />
  {:else if tab === "run"}<RunScreen />{:else}<SettingsScreen />{/if}
</main>

<style>
  nav { display: flex; gap: 0.5rem; border-bottom: 1px solid #ccc; padding: 0.5rem; }
  nav button.active { font-weight: bold; text-decoration: underline; }
  main { font-family: system-ui, sans-serif; }
</style>
```

- [ ] **Step 3: Write the four screens.** Each screen is a self-contained component that loads its data in `$effect`, renders a table, and offers a small creation form. Structure (write all four in full — the pattern below is `AssetsScreen.svelte`; the other three follow it with their own api calls):

```svelte
<script lang="ts">
  import { api, type Asset, type AssetClass, type NewAsset } from "./api";
  let classes = $state<AssetClass[]>([]);
  let assets = $state<Asset[]>([]);
  let error = $state("");
  let form = $state<NewAsset>({ asset_class_id: 0, label: "", id_kind: "ticker",
                                ticker: "", isin: null, yellow_key: "Equity" });
  let newClassName = $state("");

  async function reload() {
    try {
      classes = await api.listAssetClasses();
      assets = await api.listAssets();
      if (classes.length && !form.asset_class_id) form.asset_class_id = classes[0].id;
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  async function addClass() {
    try { await api.createAssetClass(newClassName, ""); newClassName = ""; await reload(); }
    catch (e) { error = String(e); }
  }
  async function addAsset() {
    try {
      await api.createAsset({ ...form,
        ticker: form.id_kind === "ticker" ? form.ticker : null,
        isin: form.id_kind === "isin" ? form.isin : null });
      await reload();
    } catch (e) { error = String(e); }
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
<section>
  <h2>Asset classes</h2>
  <input bind:value={newClassName} placeholder="e.g. Equity" />
  <button onclick={addClass} disabled={!newClassName}>Add class</button>
  <h2>Assets</h2>
  <form onsubmit={(e) => { e.preventDefault(); addAsset(); }}>
    <select bind:value={form.asset_class_id}>
      {#each classes as c}<option value={c.id}>{c.name}</option>{/each}
    </select>
    <input bind:value={form.label} placeholder="Label" required />
    <select bind:value={form.id_kind}>
      <option value="ticker">Ticker</option><option value="isin">ISIN</option>
    </select>
    {#if form.id_kind === "ticker"}
      <input bind:value={form.ticker} placeholder="AAPL US" required />
    {:else}
      <input bind:value={form.isin} placeholder="FR0000120271" required />
    {/if}
    <input bind:value={form.yellow_key} placeholder="Equity / Corp / Index" required />
    <button type="submit">Add asset</button>
  </form>
  <table>
    <thead><tr><th>Label</th><th>Security</th><th>Class</th><th>Active</th></tr></thead>
    <tbody>
      {#each assets as a}
        <tr>
          <td>{a.label}</td><td>{a.bdp_security}</td>
          <td>{classes.find((c) => c.id === a.asset_class_id)?.name}</td>
          <td><input type="checkbox" checked={a.active}
               onchange={() => api.setAssetActive(a.id, !a.active).then(reload)} /></td>
        </tr>
      {/each}
    </tbody>
  </table>
</section>
```

The other three screens, same idiom:
- **ViewsScreen**: view list + create form; selecting a view shows two checkbox lists (all assets / all fields) whose checked sets call `setViewAssets` / `setViewFields` on a Save button; a fields sub-form calls `createField` (inputs: class select, mnemonic, label, value_kind select). Show each view's estimate via `estimateView` next to its name.
- **RunScreen**: view select; `estimateView` result shown as `~N hits (today so far: M)` with amber styling on `SoftWarn`, red on `HardConfirm`; **Run now** button → `runEodNow(viewId, false)`; on `NeedsConfirmation` show the estimated cost and a **Confirm run** button → `runEodNow(viewId, true)`. Gap panel: `detectViewGaps` list, each range with a **Backfill** button → `runBackfillNow(viewId, start, end, false)` then the same confirm dance (backfill ALWAYS returns `NeedsConfirmation` first — spec §5.3). Below: run history table (`listRuns(50)`, refreshed on a 5-second `setInterval` inside `$effect` with cleanup), row click loads `listIssues(run.id)` into a detail panel. Status cell text: the raw status string; `failed` rows show `error_summary`.
- **SettingsScreen**: `getSettings` → editable `data_dir`, `soft_limit`, `refresh_timeout_s` → `saveSettings`. Schedules: `listSchedules` table (`view`, `window_start`, `window_end`, `drawn_at` for today, `last_result`, active toggle) + upsert form calling `upsertSchedule(viewId, "09:00", "18:00", true)` with editable time inputs.

- [ ] **Step 4: Verify** — `npm run check` → 0 errors. `npm run tauri dev` → create a class, an asset (check `bdp_security` renders resolved), a view; Run tab shows the estimate.

- [ ] **Step 5: Commit**

```bash
git add src
git commit -m "feat: svelte screens for assets, views, runs, and settings"
```

---

### Task 15: End-to-end smoke test on the Bloomberg machine

**Files:**
- Create: `docs/superpowers/specs/smoke-test-checklist.md` (the checklist below, committed for reuse)

**Interfaces:**
- Consumes: the whole app. Produces: verified pipeline (spec §8's "2-asset, 3-field view" smoke test).

This task runs on the real machine (Bloomberg Terminal logged in, Excel + add-in installed, local TimescaleDB with `bloomdata` DB created).

- [ ] **Step 1: Seed** — in the UI: class `Equity`; fields `PX_LAST` (numeric), `PX_VOLUME` (numeric), `NAME` (text); assets `AAPL US` (ticker) and one ISIN-based asset (e.g. `FR0000120271` / yellow key `Equity`); view `smoke` with both assets.
- [ ] **Step 2: Estimate** — Run tab shows ~6 estimated hits (2 assets × 3 fields), level Ok.
- [ ] **Step 3: Run** — press **Run now**. Watch: `pending/` gains the single workbook (ONE file for both assets), Excel flashes in Task Manager and exits, no orphan `EXCEL.EXE` remains.
- [ ] **Step 4: Verify DB** —
  `psql bloomdata -c "SELECT a.label, f.mnemonic, o.obs_date, o.value_num, o.value_text FROM observation o JOIN asset a ON a.id=o.asset_id JOIN field_def f ON f.id=o.field_id ORDER BY a.label, f.mnemonic;"`
  Expected: 6 rows, today's date, plausible values; run status `ok` in the history table; workbook moved into `archive/<YYYY>/<MM>/`.
- [ ] **Step 5: Idempotency in anger** — press **Run now** again; row count in `observation` unchanged (still 6), `hit_ledger` shows two entries totaling ~12.
- [ ] **Step 6: Backfill** — delete yesterday's rows is not needed; instead pick the gap panel (fresh DB shows a gap range) → **Backfill** → confirm the shown cost → verify `observation` gains weekday rows only (holidays absent — Bloomberg's calendar wins).
- [ ] **Step 7: Schedule** — Settings: schedule the `smoke` view with window `09:00–18:00`; check the `schedule` row has `drawn_for = today` and a `drawn_at` inside the window; restart the app; `drawn_at` unchanged (no re-roll).
- [ ] **Step 8: Commit the checklist**

```bash
git add docs
git commit -m "docs: end-to-end smoke test checklist"
```

---

## Execution notes

- Tasks 1–2 are sequential foundations. Tasks 3–10 are largely independent after Task 2 and can be reviewed in any order, but implement in numeric order — later tasks consume earlier interfaces. Tasks 11–14 are strictly sequential. Task 15 requires the physical Bloomberg machine.
- Postgres setup (one-time, before running ignored tests): install PostgreSQL 16 + TimescaleDB (Windows installer from timescale.com), `createdb bloomdata`, `createdb bloom_test`, set `BLOOM_TEST_DATABASE_URL`.
- The Bloomberg add-in's refresh macro name (`RefreshAllStaticData`) and the exact `#N/A` strings should be verified against the installed add-in version during Task 15; both live in exactly one place each (`refresh.ps1`, `excel_read::classify_cell`) by design.
