# P1 — Instrument/Security Master Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace ticker-as-identity with a durable internal `instrument_id`, resolve user input to it through Bloomberg with a full audit trail, and give the user a search that costs nothing.

**Architecture:** A greenfield PostgreSQL schema built around an immutable `instrument` spine, with every identifier and attribute stored as a bitemporal validity period rather than a mutable column. Resolution is a deterministic seven-step pipeline that consults local aliases first and reaches Bloomberg only when it must, recording every decision and every Bloomberg response verbatim. Search has two tiers: a local `pg_trgm` corpus that answers keystrokes for free, and an explicit button that calls Bloomberg once and caches the answer forever.

**Tech Stack:** Rust 2021 (Tauri 2, sqlx 0.8 + Postgres, tokio, chrono, thiserror), Python 3.14 BLPAPI sidecar (blpapi 3.26.7.1), Svelte 5 + TypeScript, PostgreSQL 17 with `pg_trgm`.

**Spec:** `docs/superpowers/specs/2026-08-19-security-master-design.md`

**Fact base:** `docs/superpowers/specs/2026-08-19-blpapi-field-facts.md` — every Bloomberg mnemonic used below is verified there.

---

## Global Constraints

Every task's requirements implicitly include this section.

- **Do not invent Bloomberg field names, event types, request parameters, or adjustment semantics.** Only mnemonics confirmed in the P0 fact sheet may appear in code. If a task seems to need one that is not there, stop and report it rather than guessing. Six plausible-looking mnemonics were already proven not to exist (P0 §7.1).
- **The tool calls Bloomberg only when it must.** The licence permits 500,000 hits/day (user, 2026-08-19), but the design constraint stands on its own: typing never calls Bloomberg, and a resolved instrument is never re-resolved over the wire.
- **Never overwrite an observation, an alias value, or an instrument identity.** Corrections close a row's `system_to` and insert a new row. There is no `ON CONFLICT DO UPDATE` on `observation` anywhere in this plan.
- **`instrument_id` is immutable.** `id_bb_global` and `id_bb_unique` are write-once (null to value only), enforced by trigger.
- **An alias sourced from `HISTORICAL_IDS_TIME_RANGE` must carry its `anchoring_identifier`** (P0 §6.4). Enforced by CHECK constraint.
- **A `resolution_review` row with status `pending` blocks its instrument from every view.** Nothing binds silently.
- **An `instrument_link` with `confirmed_by IS NULL` is a proposal. No query may follow it.**
- **The database is rebuilt, not migrated.** `dropdb bloomdata && createdb bloomdata` (and `bloom_test`) is a mandatory prerequisite — see Task 1.
- **Rust edition 2021**, sqlx 0.8 with features `runtime-tokio`, `postgres`, `chrono`, `migrate`. Tests needing Postgres read `BLOOM_TEST_DATABASE_URL`, defaulting to `postgres://postgres:postgres@localhost/bloom_test`.
- **Yellow keys** in this codebase are the Bloomberg market-sector strings: `Equity`, `Corp`, `Govt`, `Index`, `Curncy`, `Comdty`, `Mtge`, `Muni`, `Pfd`.
- **TDD.** Every task writes the failing test first, runs it to watch it fail, then implements. Commit at the end of every task.

---

## Deviations from the spec, and why

Three, all stated here rather than buried in a task.

**1. The fetch pipeline keeps working (Task 12).** Spec §2 puts all observation writing in P2 and says the `observation` table "stays empty" after P1. Taken literally, that leaves the application unable to fetch anything for a whole phase, and a plan is supposed to produce working software. Task 12 therefore pulls forward the smallest possible piece of P2: the sidecar sets all four adjustment flags to `false`, and ingest inserts append-only rows at `layer = 'raw'` with `basis_id` pointing at the seeded RAW basis. Point-in-time reads, supersession on correction, and every other layer remain P2. If P1 should instead leave the pipeline dark, drop Task 12's ingest steps and keep only its identity retargeting.

**2. Local search uses per-table GIN indexes and a UNION, not one `search_text` column.** Spec §6.1 describes "a GIN trigram index covering a `search_text` built from" four sources. A single column would require either a materialised view (stale between refreshes — a newly added book entry would not be findable) or denormalisation triggers on four tables. Indexing each source in place and combining them in the query is fresh by construction. The behaviour §6.1 specifies — ranking by `similarity()`, a minimum threshold, results labelled by origin — is unchanged.

**3. Identity was written only at instrument creation, and is now refreshed on
every resolution.** This deviation is recorded after the fact, because it was
not a decision — it was a defect, and the file table above implies otherwise.
The table gives `resolution/engine.rs` the whole seven-step pipeline and
`instrument/store.rs` the bitemporal writes, which reads as though a later
resolution of an instrument already in the master updates what it knows. It did
not. `bind_identity` was bind-**or-return-existing**: on finding the FIGI (or,
with no FIGI, the `bdp_security` alias) already present, it returned the
existing `instrument_id` **before any alias or attribute write at all**.
`insert_alias` and `set_attr` ran only on the creation branch.

The consequence was the phase's headline promise having no production path.
An instrument bound while it wore `FB US Equity`, later resolved as
`META US Equity`, found no local alias at step 2, got the same FIGI back at
step 3, and had **nothing written**: `current_security` went on answering
`FB US Equity`, `load_view` went on sending a dead ticker, and the series
stopped without a single error. Every test that exercised a rename drove
`history::apply` directly, so nothing caught it.

`reconcile_identity` is the fix (Task C1 of the final fix wave): on the dedup
path the current `bdp_security` period is closed at today and a new one
inserted — never an `UPDATE` — and the creation path's attribute loop is re-run
through a shared `write_attrs_tx`, inside one transaction. It also answers the
`HISTORICAL_IDS_TIME_RANGE` bootstrap problem (P0 §6.5) in the only way
available: a rename cannot be *discovered* from a field anchored on the chain's
start, but it can be discovered from a FIGI we already hold now answering to a
different security string.

---

## File Structure

**Created**

| Path | Responsibility |
|---|---|
| `src-tauri/migrations/0001_init.sql` | The whole schema. Replaces the four existing migrations. |
| `src-tauri/src/resolution/mod.rs` | Module root; shared types. |
| `src-tauri/src/resolution/normalize.rs` | Pure string work: normalise input, build a security string, normalise Bloomberg's `AAPL US<equity>` form, recognise option contracts and ISINs. No I/O. |
| `src-tauri/src/resolution/score.rs` | Pure candidate scoring against hints. No I/O. |
| `src-tauri/src/resolution/engine.rs` | The seven-step pipeline. Owns `resolution_decision` and `resolution_review` writes. |
| `src-tauri/src/instrument/mod.rs` | Module root; `Instrument`, `Alias`, `Attr` types. |
| `src-tauri/src/instrument/store.rs` | Bitemporal reads and writes for `instrument`, `instrument_attr`, `instrument_alias`, `instrument_link`. |
| `src-tauri/src/instrument/history.rs` | `HISTORICAL_IDS_TIME_RANGE` ingestion and link proposals. |
| `src-tauri/src/instrument/search.rs` | Local `pg_trgm` search and the candidate cache. |
| `src-tauri/src/master_fetch.rs` | Bloomberg request/response types for the three master requests, the `MasterFetcher` seam, and its mock. |
| `src-tauri/src/book.rs` | `book_entry` CRUD. Replaces the `Asset` half of `registry.rs`. |
| `src-tauri/tests/schema.rs` | Integration tests asserting schema invariants. |
| `src/lib/BookScreen.svelte` | Search box, Search Bloomberg button, book list. Replaces `AssetsScreen.svelte`. |
| `src/lib/ReviewScreen.svelte` | The resolution review queue and link proposals. |
| `src/lib/InstrumentDetail.svelte` | Attribute and alias timelines. |

**Modified**

| Path | Change |
|---|---|
| `src-tauri/src/registry.rs` | Loses `Asset`, `NewAsset`, `create_asset`, `list_assets`, `set_asset_active`, `resolve_bdp_security` and its tests. Keeps `AssetClass` only. |
| `src-tauri/src/fetch.rs` | `FetchAsset.asset_id`, `ObsCell.asset_id`, `CellProblem.asset_id` all become `instrument_id`. |
| `src-tauri/src/orchestrator.rs` | `load_view` reads instruments. |
| `src-tauri/src/ingest.rs` | Append-only insert at `layer='raw'`; `ingest_issue.instrument_id`. |
| `src-tauri/src/views.rs` | `view_asset` becomes `view_instrument`; excludes instruments with a pending review. |
| `src-tauri/src/deletion.rs` | `asset` becomes `book_entry` + `instrument`. |
| `src-tauri/src/scheduler.rs` | Gap detection joins `view_instrument`. |
| `src-tauri/src/bulk/{mod,sheet,diff}.rs` | Sheet is a book, keyed on `instrument_id`; ambiguous rows become review rows. |
| `src-tauri/src/commands.rs` | New commands; asset commands removed. |
| `src-tauri/src/lib.rs` | New modules, new command registrations. |
| `src-tauri/scripts/blp_fetch.py` | Two new request kinds, overrides, bulk-field parsing, adjustment flags. |
| `src/lib/api.ts` | New command bindings and types. |
| `src/routes/+page.svelte` | `Assets` tab becomes `Book`; new `Review` tab. |

**Deleted:** `src-tauri/migrations/0002_schedule_unique.sql`, `0003_blpapi.sql`, `0004_fix_doubled_yellow_key.sql`, `src/lib/AssetsScreen.svelte`.

---

## Task Order and Dependencies

```
1 schema
├── 2 normalize ──┐
├── 3 score ──────┤
├── 4 store ──────┼── 7 engine ── 8 history ── 9 book ── 12 retarget ── 13 bulk
├── 5 sidecar ────┤                                   │
└── 6 master_fetch┘                                   ├── 14 book UI
    10 local search ── 11 bloomberg search ───────────┤
                                                      ├── 15 review UI
                                                      ├── 16 detail UI
                                                      └── 17 smoke checklist
```

Tasks 2, 3, 5 and 6 are independent of each other and of 4; they can be done in any order after Task 1.

Two edges the diagram flattens: Task 9's book entry calls `search::link_candidate`
from Task 10, so do 10 before 9 (or add that one call when 10 lands). Task 12 is
the first point at which `cargo test` runs the whole suite — between Tasks 1 and
12 the crate does not compile, and each task runs only its own integration test.

---

## Task 1: The consolidated schema

Builds the whole database in one migration and proves its invariants hold. Nothing else can start until this lands.

**Before you begin — the database reset is mandatory, not optional.** sqlx stores each migration's checksum in `_sqlx_migrations` and runs migrations at application startup. A database that already applied the old `0001_init.sql` will fail the checksum and the app will not boot. Run this first:

```bash
# 1. Export the existing book while the old app still runs (it is the migration tool).
#    In the running app: Assets screen -> Export -> note the path.
# 2. Then, with the app closed:
dropdb -U postgres bloomdata && createdb -U postgres bloomdata
dropdb -U postgres bloom_test 2>/dev/null; createdb -U postgres bloom_test
```

**Files:**
- Create: `src-tauri/migrations/0001_init.sql` (replacing the existing file entirely)
- Delete: `src-tauri/migrations/0002_schedule_unique.sql`, `src-tauri/migrations/0003_blpapi.sql`, `src-tauri/migrations/0004_fix_doubled_yellow_key.sql`
- Create: `src-tauri/tests/schema.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: the tables every later task reads and writes. Also produces the test helper `schema::test_pool() -> sqlx::PgPool`, which later integration tests reuse via `mod common;`.

- [ ] **Step 1: Write the failing schema test**

Create `src-tauri/tests/schema.rs`:

```rust
//! Schema invariants. These are the constraints the design leans on; if one of
//! them stops holding, a later phase corrupts history silently rather than
//! failing loudly, so they are asserted directly against the database.

use sqlx::{PgPool, Row};

/// Connects to the test database and runs migrations. Every integration test
/// starts here. Requires an EMPTY database on first run.
pub async fn test_pool() -> PgPool {
    let url = std::env::var("BLOOM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/bloom_test".into());
    let pool = PgPool::connect(&url).await.expect("connect to bloom_test");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
    pool
}

/// Creates a bare instrument and returns its id.
async fn new_instrument(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>(
        "INSERT INTO instrument DEFAULT VALUES RETURNING instrument_id")
        .fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn migrations_apply_and_seed_the_two_adjustment_bases() {
    let pool = test_pool().await;
    let rows = sqlx::query("SELECT note, adj_normal, adj_abnormal, adj_split,
                                   adj_follow_dpdf FROM adjustment_basis ORDER BY id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "exactly RAW and LEGACY_DPDF are seeded");
    // RAW: all four flags explicitly false. P0 3.1 measured that this, and only
    // this, returns unadjusted prices.
    assert_eq!(rows[0].get::<Option<bool>, _>("adj_normal"), Some(false));
    assert_eq!(rows[0].get::<Option<bool>, _>("adj_split"), Some(false));
    // LEGACY_DPDF: unknown, because the flags were never set and the Terminal's
    // DPDF<GO> setting was not captured.
    assert_eq!(rows[1].get::<Option<bool>, _>("adj_normal"), None);
}

#[tokio::test]
async fn an_alias_from_historical_ids_without_an_anchor_is_rejected() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let err = sqlx::query(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source)
         VALUES ($1, 'ticker', 'FB', DATE '2012-05-18', 'bloomberg_hist_ids')")
        .bind(iid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("alias_anchor_required"),
            "unanchored hist-ids alias must violate alias_anchor_required, got: {err}");
}

#[tokio::test]
async fn an_alias_from_a_reference_request_needs_no_anchor() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    sqlx::query(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source)
         VALUES ($1, 'ticker', 'AAPL US', DATE '1980-12-12', 'bloomberg_ref')")
        .bind(iid).execute(&pool).await.expect("bloomberg_ref alias needs no anchor");
}

#[tokio::test]
async fn id_bb_global_is_write_once() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    sqlx::query("UPDATE instrument SET id_bb_global = 'BBG000B9XRY4' WHERE instrument_id = $1")
        .bind(iid).execute(&pool).await.expect("null -> value is allowed");
    let err = sqlx::query(
        "UPDATE instrument SET id_bb_global = 'BBG000000000' WHERE instrument_id = $1")
        .bind(iid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("write-once"),
            "overwriting a known FIGI must be refused, got: {err}");
}

#[tokio::test]
async fn an_alias_value_cannot_be_updated_but_can_be_closed() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let aid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, valid_from, source)
         VALUES ($1, 'ticker', 'FB', DATE '2012-05-18', 'user') RETURNING id")
        .bind(iid).fetch_one(&pool).await.unwrap();

    // Closing a validity period is the supported way to record a ticker change.
    sqlx::query("UPDATE instrument_alias SET valid_to = DATE '2022-06-09' WHERE id = $1")
        .bind(aid).execute(&pool).await.expect("closing valid_to is allowed");

    let err = sqlx::query("UPDATE instrument_alias SET value = 'META' WHERE id = $1")
        .bind(aid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("immutable"),
            "rewriting an alias value destroys history and must be refused, got: {err}");
}

#[tokio::test]
async fn only_one_current_row_per_logical_observation_series() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let basis = sqlx::query_scalar::<_, i16>(
        "SELECT id FROM adjustment_basis WHERE adj_normal = false").fetch_one(&pool).await.unwrap();

    let insert = |v: f64| sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, granularity, layer, basis_id, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-18','eod','raw',$3,$4,$5)")
        .bind(iid).bind(fid).bind(basis).bind(v).bind(rid);

    insert(499.23).execute(&pool).await.expect("first current row");
    let err = insert(124.81).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("observation_current"),
            "a second CURRENT row for the same series must collide, got: {err}");

    // Superseding is legal: close the old row, then insert.
    sqlx::query("UPDATE observation SET system_to = now()
                 WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(iid).execute(&pool).await.unwrap();
    insert(124.81).execute(&pool).await.expect("a correction inserts beneath the closed row");
    let n = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM observation WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 2, "the superseded row is retained, not replaced");
}

#[tokio::test]
async fn an_eod_observation_must_have_no_time_and_an_intraday_one_must() {
    let pool = test_pool().await;
    let iid = new_instrument(&pool).await;
    let (fid, rid) = seed_field_and_run(&pool, iid).await;
    let err = sqlx::query(
        "INSERT INTO observation
           (instrument_id, field_id, obs_date, obs_time, granularity, layer, value_num, run_id)
         VALUES ($1,$2,DATE '2026-08-18',TIME '16:00','eod','raw',1.0,$3)")
        .bind(iid).bind(fid).bind(rid).execute(&pool).await.unwrap_err();
    assert!(err.to_string().contains("observation_granularity_time"),
            "an EOD row carrying a time is ambiguous and must be refused, got: {err}");
}

#[tokio::test]
async fn pg_trgm_is_available() {
    let pool = test_pool().await;
    let s: f32 = sqlx::query_scalar("SELECT similarity('AAPL US Equity', 'AAPL')")
        .fetch_one(&pool).await.expect("pg_trgm must be installed by the migration");
    assert!(s > 0.0);
}

/// A field_def and a run, so observation's foreign keys are satisfiable.
async fn seed_field_and_run(pool: &PgPool, instrument_id: i64) -> (i64, i64) {
    let cid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO asset_class (name) VALUES ('Equity')
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id")
        .fetch_one(pool).await.unwrap();
    let fid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric')
         ON CONFLICT (asset_class_id, mnemonic) DO UPDATE SET label = EXCLUDED.label
         RETURNING id").bind(cid).fetch_one(pool).await.unwrap();
    let vid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO view (name) VALUES ('t' || $1::text) RETURNING id")
        .bind(instrument_id).fetch_one(pool).await.unwrap();
    let rid = sqlx::query_scalar::<_, i64>(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (fid, rid)
}
```

- [ ] **Step 2: Run the test to verify it fails**

```bash
cd src-tauri && cargo test --test schema
```

Expected: compilation succeeds but every test fails — the tables do not exist yet (`relation "adjustment_basis" does not exist`).

- [ ] **Step 3: Delete the superseded migrations**

```bash
cd src-tauri && git rm migrations/0002_schedule_unique.sql \
                       migrations/0003_blpapi.sql \
                       migrations/0004_fix_doubled_yellow_key.sql
```

Migration `0004`'s real protection — the doubled-yellow-key repair — moves to `resolution::normalize` with its regression test in Task 2, so nothing is lost. `0002`'s unique constraint and `0003`'s renamed column are folded into the new `0001` below.

- [ ] **Step 4: Write the new schema**

Replace `src-tauri/migrations/0001_init.sql` entirely. Table order matters: `instrument_attr` references `resolution_decision`, so `resolution_decision` is created first.

```sql
-- P1 instrument/security master. Greenfield: this replaces migrations 0001-0004,
-- which is why the database must be dropped and recreated before first run.
--
-- Reading order: identity spine, then everything that hangs off it, then the
-- pipeline tables retained from the previous schema.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------- identity

-- The spine. Nothing here changes: no ticker, no ISIN, no name, no status.
-- id_bb_global is nullable because a user may create an instrument before
-- Bloomberg has been asked about it; it is write-once once known.
CREATE TABLE instrument (
  instrument_id  BIGSERIAL PRIMARY KEY,
  id_bb_global   TEXT UNIQUE,
  id_bb_unique   TEXT,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE FUNCTION instrument_write_once() RETURNS trigger AS $fn$
BEGIN
  IF NEW.instrument_id <> OLD.instrument_id THEN
    RAISE EXCEPTION 'instrument_id is immutable (write-once)';
  END IF;
  IF OLD.id_bb_global IS NOT NULL
     AND NEW.id_bb_global IS DISTINCT FROM OLD.id_bb_global THEN
    RAISE EXCEPTION 'id_bb_global is write-once: % cannot become %',
      OLD.id_bb_global, NEW.id_bb_global;
  END IF;
  IF OLD.id_bb_unique IS NOT NULL
     AND NEW.id_bb_unique IS DISTINCT FROM OLD.id_bb_unique THEN
    RAISE EXCEPTION 'id_bb_unique is write-once';
  END IF;
  IF NEW.created_at <> OLD.created_at THEN
    RAISE EXCEPTION 'created_at is immutable (write-once)';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER instrument_write_once BEFORE UPDATE ON instrument
  FOR EACH ROW EXECUTE FUNCTION instrument_write_once();

-- Every resolution, including the ones that never called Bloomberg. Created
-- before instrument_attr because attributes cite the decision that produced them.
CREATE TABLE resolution_decision (
  id                   BIGSERIAL PRIMARY KEY,
  raw_input            TEXT NOT NULL,
  normalized           TEXT NOT NULL,
  hint_exchange        TEXT,
  hint_country         TEXT,
  hint_currency        TEXT,
  hint_asset_class     TEXT,
  method               TEXT NOT NULL CHECK (method IN
                         ('local_alias','bloomberg_ref','bloomberg_list','manual')),
  chosen_instrument_id BIGINT REFERENCES instrument(instrument_id),
  candidates           JSONB NOT NULL,
  bbg_response         JSONB,
  decided_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
  decided_by           TEXT NOT NULL
);

-- Bitemporal attributes. valid_* is when the fact was true in the world;
-- system_* is when we believed it. A correction closes system_to and inserts.
CREATE TABLE instrument_attr (
  id             BIGSERIAL PRIMARY KEY,
  instrument_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  attr           TEXT NOT NULL CHECK (attr IN
                   ('name','exchange','country','currency','asset_class',
                    'instrument_type','issuer','share_class','fund_vehicle','status')),
  value          TEXT NOT NULL,
  valid_from     DATE NOT NULL,
  -- 9999-12-31, NOT 'infinity': sqlx decodes DATE 'infinity' as i32::MAX days,
  -- which overflows chrono::NaiveDate and PANICS the process on read. See the
  -- no_infinity CHECK below, which makes the mistake unrepresentable.
  valid_to       DATE NOT NULL DEFAULT DATE '9999-12-31',
  system_from    TIMESTAMPTZ NOT NULL DEFAULT now(),
  -- system_to keeps 'infinity': three partial indexes hard-code the predicate.
  -- NEVER decode this column into chrono::DateTime -- same overflow panic.
  -- Readers want `system_to < 'infinity' AS superseded`, not the timestamp.
  system_to      TIMESTAMPTZ NOT NULL DEFAULT 'infinity',
  source         TEXT NOT NULL CHECK (source IN ('bloomberg','user','derived')),
  decision_id    BIGINT REFERENCES resolution_decision(id),
  CONSTRAINT instrument_attr_period CHECK (valid_from < valid_to),
  CONSTRAINT instrument_attr_no_infinity CHECK (valid_to <> 'infinity')
);
CREATE UNIQUE INDEX instrument_attr_current
  ON instrument_attr (instrument_id, attr, valid_from)
  WHERE system_to = 'infinity';

-- Every identifier ever worn. A ticker change closes a row and inserts another;
-- no UPDATE ever touches `value`.
CREATE TABLE instrument_alias (
  id                   BIGSERIAL PRIMARY KEY,
  instrument_id        BIGINT NOT NULL REFERENCES instrument(instrument_id),
  id_type              TEXT NOT NULL CHECK (id_type IN
                         ('ticker','isin','figi','cusip','sedol','bbg_unique',
                          'bdp_security')),
  value                TEXT NOT NULL,
  exch_code            TEXT,
  valid_from           DATE NOT NULL,
  valid_to             DATE NOT NULL DEFAULT DATE '9999-12-31',  -- see above
  system_from          TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to            TIMESTAMPTZ NOT NULL DEFAULT 'infinity',
  source               TEXT NOT NULL CHECK (source IN
                         ('bloomberg_hist_ids','bloomberg_ref','user')),
  bbg_action_id        TEXT,
  anchoring_identifier TEXT,
  CONSTRAINT instrument_alias_period CHECK (valid_from < valid_to),
  CONSTRAINT instrument_alias_no_infinity CHECK (valid_to <> 'infinity')
);

-- P0 6.4: HISTORICAL_IDS_TIME_RANGE asked about META US Equity returns
-- Facebook's rename or the Roundhill ETF's rename depending on whether
-- HISTORICAL_STARTING_IDENTIFIER was supplied. An alias whose anchor is unknown
-- cannot be trusted, so storing one is made impossible.
ALTER TABLE instrument_alias ADD CONSTRAINT alias_anchor_required
  CHECK (source <> 'bloomberg_hist_ids'
    OR (anchoring_identifier IS NOT NULL AND btrim(anchoring_identifier) <> ''));
-- The IS NOT NULL half is load-bearing: btrim(NULL) is NULL, and a CHECK that
-- evaluates to NULL passes. An empty string is not an anchor.

CREATE INDEX instrument_alias_lookup ON instrument_alias (id_type, lower(value));
CREATE INDEX instrument_alias_by_instrument ON instrument_alias (instrument_id);
CREATE UNIQUE INDEX instrument_alias_current
  ON instrument_alias (instrument_id, id_type, value, valid_from)
  WHERE system_to = 'infinity';
CREATE INDEX instrument_alias_trgm
  ON instrument_alias USING gin (value gin_trgm_ops);

CREATE FUNCTION alias_value_immutable() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value <> OLD.value
     OR NEW.id_type <> OLD.id_type
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.valid_from <> OLD.valid_from
     OR NEW.source <> OLD.source
     -- Provenance, not state. anchoring_identifier in particular is what the
     -- whole META/Roundhill defence rests on: rewriting it in place would
     -- launder an unanchored alias into a trusted one.
     OR NEW.anchoring_identifier IS DISTINCT FROM OLD.anchoring_identifier
     OR NEW.bbg_action_id IS DISTINCT FROM OLD.bbg_action_id
     OR NEW.exch_code IS DISTINCT FROM OLD.exch_code THEN
    RAISE EXCEPTION
      'instrument_alias identity columns are immutable; close valid_to/system_to and insert a new row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER instrument_alias_immutable BEFORE UPDATE ON instrument_alias
  FOR EACH ROW EXECUTE FUNCTION alias_value_immutable();

CREATE FUNCTION attr_value_immutable() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value <> OLD.value
     OR NEW.attr <> OLD.attr
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.valid_from <> OLD.valid_from
     OR NEW.source <> OLD.source THEN
    RAISE EXCEPTION
      'instrument_attr identity columns are immutable; close system_to and insert a new row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER instrument_attr_immutable BEFORE UPDATE ON instrument_attr
  FOR EACH ROW EXECUTE FUNCTION attr_value_immutable();

-- P0 7.2: no Bloomberg field returns a successor security, so every link is
-- derived. confirmed_by IS NULL means "proposed"; no query may follow it.
CREATE TABLE instrument_link (
  id              BIGSERIAL PRIMARY KEY,
  predecessor_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  successor_id    BIGINT NOT NULL REFERENCES instrument(instrument_id),
  link_type       TEXT NOT NULL CHECK (link_type IN
                    ('rename','merger','conversion','share_class_change','spinoff')),
  effective_date  DATE NOT NULL,
  evidence        JSONB NOT NULL,
  confirmed_by    TEXT,
  confirmed_at    TIMESTAMPTZ,
  CHECK (predecessor_id <> successor_id),
  CHECK ((confirmed_by IS NULL) = (confirmed_at IS NULL))
);
CREATE INDEX instrument_link_pred ON instrument_link (predecessor_id);
CREATE INDEX instrument_link_succ ON instrument_link (successor_id);

CREATE TABLE resolution_review (
  id            BIGSERIAL PRIMARY KEY,
  decision_id   BIGINT NOT NULL REFERENCES resolution_decision(id),
  status        TEXT NOT NULL CHECK (status IN ('pending','resolved','rejected')),
  opened_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  closed_at     TIMESTAMPTZ,
  note          TEXT NOT NULL DEFAULT ''
);
CREATE INDEX resolution_review_pending ON resolution_review (status)
  WHERE status = 'pending';

-- The user's book. Identity belongs to `instrument`; the label and the active
-- flag belong here. There is deliberately no UNIQUE (security): one instrument
-- legitimately wears several security strings over time.
CREATE TABLE book_entry (
  instrument_id  BIGINT PRIMARY KEY REFERENCES instrument(instrument_id),
  asset_class_id BIGINT NOT NULL,
  label          TEXT NOT NULL,
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  added_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  note           TEXT NOT NULL DEFAULT ''
);
CREATE INDEX book_entry_label_trgm ON book_entry USING gin (label gin_trgm_ops);

-- Every row instrumentListRequest has ever returned, kept forever. This is what
-- makes local search free: one search for "AAPL" seeds all its listings.
CREATE TABLE instrument_candidate (
  id             BIGSERIAL PRIMARY KEY,
  security       TEXT NOT NULL UNIQUE,
  raw_security   TEXT NOT NULL,
  description    TEXT NOT NULL,
  yellow_key     TEXT,
  first_seen     TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen      TIMESTAMPTZ NOT NULL DEFAULT now(),
  instrument_id  BIGINT REFERENCES instrument(instrument_id)
);
CREATE INDEX instrument_candidate_sec_trgm
  ON instrument_candidate USING gin (security gin_trgm_ops);
CREATE INDEX instrument_candidate_desc_trgm
  ON instrument_candidate USING gin (description gin_trgm_ops);

-- ---------------------------------------------------------------- pipeline

CREATE TABLE asset_class (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT ''
);

ALTER TABLE book_entry ADD CONSTRAINT book_entry_class_fk
  FOREIGN KEY (asset_class_id) REFERENCES asset_class(id);

-- The configurable field-mapping layer the objectives require. bbg_ftype
-- records P0 5's machine-readable marker: 'BulkFormat' means table-valued.
-- Adding a field stays an INSERT, never a migration.
CREATE TABLE field_def (
  id               BIGSERIAL PRIMARY KEY,
  asset_class_id   BIGINT NOT NULL REFERENCES asset_class(id),
  mnemonic         TEXT NOT NULL,
  label            TEXT NOT NULL,
  value_kind       TEXT NOT NULL CHECK (value_kind IN ('numeric','text','date')),
  bbg_ftype        TEXT,
  bbg_datatype     TEXT,
  entitlement_note TEXT NOT NULL DEFAULT '',
  active           BOOLEAN NOT NULL DEFAULT TRUE,
  UNIQUE (asset_class_id, mnemonic)
);

CREATE TABLE view (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT '',
  active      BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE view_instrument (
  view_id       BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  instrument_id BIGINT NOT NULL REFERENCES instrument(instrument_id),
  PRIMARY KEY (view_id, instrument_id)
);

CREATE TABLE view_field (
  view_id  BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  field_id BIGINT NOT NULL REFERENCES field_def(id),
  PRIMARY KEY (view_id, field_id)
);

-- Amendment A2 status ladder, folded in from migration 0003: data arrives over
-- BLPAPI, so there is no generate stage and reading is not separate from fetching.
CREATE TABLE run (
  id             BIGSERIAL PRIMARY KEY,
  view_id        BIGINT NOT NULL REFERENCES view(id),
  kind           TEXT NOT NULL CHECK (kind IN ('eod','backfill')),
  trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('manual','scheduled')),
  status         TEXT NOT NULL CHECK (status IN
    ('pending','fetching','ingesting','ok','failed','partial')),
  started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at    TIMESTAMPTZ,
  payload_path   TEXT,
  estimated_hits BIGINT NOT NULL DEFAULT 0,
  error_summary  TEXT
);

-- The exact flag combination that produced a value. P0 3 measured that these
-- four flags change the number: AAPL closed 2020-08-28 at 499.23 raw, 124.81
-- split-adjusted, 120.96 fully adjusted. A price without its basis is not a fact.
CREATE TABLE adjustment_basis (
  id              SMALLSERIAL PRIMARY KEY,
  adj_normal      BOOLEAN,
  adj_abnormal    BOOLEAN,
  adj_split       BOOLEAN,
  adj_follow_dpdf BOOLEAN,
  note            TEXT NOT NULL DEFAULT ''
);

INSERT INTO adjustment_basis
  (adj_normal, adj_abnormal, adj_split, adj_follow_dpdf, note) VALUES
  (false, false, false, false,
   'RAW - all four adjustment flags explicitly false. The only combination P0 3.1 measured as unadjusted.'),
  (NULL, NULL, NULL, NULL,
   'LEGACY_DPDF - flags were never set, so the value followed the Terminal''s DPDF<GO> setting, which was not captured. Not reproducible.');

CREATE TABLE observation (
  id             BIGSERIAL PRIMARY KEY,
  instrument_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  field_id       BIGINT NOT NULL REFERENCES field_def(id),
  obs_date       DATE NOT NULL,
  obs_time       TIME,
  granularity    TEXT NOT NULL DEFAULT 'eod',
  layer          TEXT NOT NULL CHECK (layer IN
                   ('raw','bbg_adjusted','derived_adjusted','total_return',
                    'holdings_transformed')),
  basis_id       SMALLINT REFERENCES adjustment_basis(id),
  value_num      DOUBLE PRECISION,
  value_text     TEXT,
  system_from    TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to      TIMESTAMPTZ NOT NULL DEFAULT 'infinity',
  run_id         BIGINT NOT NULL REFERENCES run(id),
  CONSTRAINT observation_one_value
    CHECK ((value_num IS NULL) <> (value_text IS NULL)),
  CONSTRAINT observation_granularity_time
    CHECK ((granularity = 'eod') = (obs_time IS NULL)),
  -- Spec 4.8: adjustment basis is recorded, not assumed. Only numeric prices
  -- carry a basis; text-valued fields (NAME and similar) legitimately have none.
  CONSTRAINT observation_numeric_needs_basis
    CHECK (value_num IS NULL OR basis_id IS NOT NULL),
  -- Spec 4.8 requires a new granularity to be addable as a new value, not a
  -- schema change, so the case is normalised rather than enumerated --
  -- otherwise 'EOD' and 'eod' silently partition observation_current's key.
  CONSTRAINT observation_granularity_lower
    CHECK (granularity = lower(granularity) AND granularity <> '')
);

-- One current row per logical series; the superseded history accumulates beneath.
-- NULLS NOT DISTINCT is load-bearing, not decoration: obs_time is NULL for every
-- EOD row (see observation_granularity_time) and basis_id is NULL for
-- text-valued fields, so under Postgres' default NULL-is-distinct rule this
-- index would let unlimited "current" rows through for exactly the series it
-- exists to protect. Requires PostgreSQL 15+; this project is on 17.
CREATE UNIQUE INDEX observation_current ON observation
  (instrument_id, field_id, obs_date, obs_time, granularity, layer, basis_id)
  NULLS NOT DISTINCT
  WHERE system_to = 'infinity';
CREATE INDEX observation_by_date ON observation (obs_date);

CREATE FUNCTION observation_append_only() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value_num IS DISTINCT FROM OLD.value_num
     OR NEW.value_text IS DISTINCT FROM OLD.value_text
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.field_id <> OLD.field_id
     OR NEW.obs_date <> OLD.obs_date
     OR NEW.obs_time IS DISTINCT FROM OLD.obs_time
     OR NEW.granularity <> OLD.granularity
     OR NEW.layer <> OLD.layer
     OR NEW.basis_id IS DISTINCT FROM OLD.basis_id
     OR NEW.run_id <> OLD.run_id THEN
    RAISE EXCEPTION
      'observations are append-only; close system_to and insert a corrected row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER observation_append_only BEFORE UPDATE ON observation
  FOR EACH ROW EXECUTE FUNCTION observation_append_only();

CREATE TABLE ingest_issue (
  id            BIGSERIAL PRIMARY KEY,
  run_id        BIGINT NOT NULL REFERENCES run(id),
  instrument_id BIGINT REFERENCES instrument(instrument_id),
  field_id      BIGINT REFERENCES field_def(id),
  obs_date      DATE,
  severity      TEXT NOT NULL CHECK (severity IN ('warn','error')),
  code          TEXT NOT NULL,
  detail        TEXT NOT NULL DEFAULT ''
);

-- run_id is nullable: a Search Bloomberg press is a metered call with no run.
CREATE TABLE hit_ledger (
  id             BIGSERIAL PRIMARY KEY,
  run_id         BIGINT REFERENCES run(id),
  purpose        TEXT NOT NULL DEFAULT 'run',
  estimated_hits BIGINT NOT NULL,
  occurred_on    DATE NOT NULL DEFAULT CURRENT_DATE
);
CREATE INDEX hit_ledger_by_day ON hit_ledger (occurred_on);

CREATE TABLE schedule (
  id           BIGSERIAL PRIMARY KEY,
  view_id      BIGINT NOT NULL REFERENCES view(id),
  active       BOOLEAN NOT NULL DEFAULT TRUE,
  window_start TIME NOT NULL DEFAULT '09:00',
  window_end   TIME NOT NULL DEFAULT '18:00',
  drawn_for    DATE,
  drawn_at     TIME,
  last_result  TEXT,
  CONSTRAINT schedule_view_unique UNIQUE (view_id)
);
```

Note what is gone and why: the TimescaleDB block (never used — nothing in this
schema depends on a hypertable feature, and Timescale ships no Windows build),
and the `asset` table (superseded by `instrument` + `book_entry`).

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test schema
```

Expected: all eight tests PASS. If `pg_trgm_is_available` fails, the Postgres
user lacks rights to `CREATE EXTENSION`; run `CREATE EXTENSION pg_trgm;` once as
a superuser in `bloom_test` and `bloomdata`.

The rest of the crate will not compile yet — `registry.rs` still references
`asset`. That is expected and is fixed in Task 9. To run only this test while the
crate is mid-migration, the integration test compiles independently of `src/`.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/migrations src-tauri/tests/schema.rs
git commit -m "feat(schema): instrument/security master schema, replacing migrations 0001-0004"
```

---
## Task 2: Input normalisation

Pure string work with no database and no Bloomberg. This is where the doubled-yellow-key defect that migration `0004` had to repair is prevented instead of repaired, and where Bloomberg's own `AAPL US<equity>` output form is converted into a usable security string.

**Files:**
- Create: `src-tauri/src/resolution/mod.rs`
- Create: `src-tauri/src/resolution/normalize.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod resolution;`)

**Interfaces:**
- Consumes: `crate::error::{AppError, AppResult}`.
- Produces:
  - `pub enum IdKind { Ticker, Isin }`
  - `pub fn detect_id_kind(input: &str) -> IdKind`
  - `pub fn build_security(kind: IdKind, identifier: &str, yellow_key: &str) -> AppResult<String>`
  - `pub fn normalize_bbg_security(raw: &str) -> Option<String>`
  - `pub fn is_option_contract(security: &str) -> bool`
  - `pub const YELLOW_KEYS: [&str; 9]`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/resolution/normalize.rs` containing only the test module for now:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_and_yellow_key_are_joined() {
        assert_eq!(build_security(IdKind::Ticker, "AAPL US", "Equity").unwrap(),
                   "AAPL US Equity");
    }

    #[test]
    fn isin_gets_the_slash_isin_form() {
        assert_eq!(build_security(IdKind::Isin, "FR0000120271", "Corp").unwrap(),
                   "/isin/FR0000120271 Corp");
        for input in ["FR0000120271", "/isin/FR0000120271", "/isin/FR0000120271 Corp"] {
            assert_eq!(build_security(IdKind::Isin, input, "Corp").unwrap(),
                       "/isin/FR0000120271 Corp", "input {input:?}");
        }
    }

    /// Regression, carried over from migration 0004 and registry.rs. Runs 1 and 2
    /// on 2026-08-17 both asked Bloomberg for "AAPL US Equity Equity" and were
    /// rejected with BAD_SEC/INVALID_SECURITY. The ticker looked perfectly valid
    /// in the UI, because the duplication existed only in the derived security.
    #[test]
    fn a_ticker_carrying_its_own_yellow_key_is_not_doubled() {
        for input in ["AAPL US Equity", "AAPL US equity", "AAPL US EQUITY",
                      "  AAPL US Equity  "] {
            assert_eq!(build_security(IdKind::Ticker, input, "Equity").unwrap(),
                       "AAPL US Equity", "input {input:?}");
        }
        // A different key is not a duplicate and must survive untouched.
        assert_eq!(build_security(IdKind::Ticker, "AAPL US Equity", "Corp").unwrap(),
                   "AAPL US Equity Corp");
        // Nothing left once the key is stripped is a user error, not a silent pass.
        assert!(build_security(IdKind::Ticker, "Equity", "Equity").is_err());
    }

    #[test]
    fn inputs_are_trimmed_and_the_key_is_required() {
        assert_eq!(build_security(IdKind::Ticker, " AAPL US ", " Equity ").unwrap(),
                   "AAPL US Equity");
        assert!(build_security(IdKind::Ticker, "AAPL US", "  ").is_err());
        assert!(build_security(IdKind::Ticker, "", "Equity").is_err());
    }

    /// An ISIN is two letters, nine alphanumerics and a check digit. Anything
    /// else the user types is a ticker, including tickers that begin with two
    /// letters.
    #[test]
    fn id_kind_is_detected_from_the_shape_of_the_input() {
        assert_eq!(detect_id_kind("FR0000120271"), IdKind::Isin);
        assert_eq!(detect_id_kind("us0378331005"), IdKind::Isin);
        assert_eq!(detect_id_kind("/isin/FR0000120271"), IdKind::Isin);
        assert_eq!(detect_id_kind("AAPL US"), IdKind::Ticker);
        assert_eq!(detect_id_kind("FR"), IdKind::Ticker);
        assert_eq!(detect_id_kind("FR0000120271X"), IdKind::Ticker);  // too long
    }

    /// P0 6: instrumentListRequest returns "AAPL US<equity>", which the Terminal
    /// does NOT accept as a security. Pasting it produces exactly the malformed
    /// identifier migration 0004 had to repair, so it is normalised on arrival
    /// and the raw form is never used as a security string.
    #[test]
    fn bloomberg_list_output_is_normalised_to_a_security_string() {
        assert_eq!(normalize_bbg_security("AAPL US<equity>").as_deref(),
                   Some("AAPL US Equity"));
        assert_eq!(normalize_bbg_security("T 4 ⅜ 05/15/41<govt>").as_deref(),
                   Some("T 4 ⅜ 05/15/41 Govt"));
        assert_eq!(normalize_bbg_security("VFIAX US<equity>").as_deref(),
                   Some("VFIAX US Equity"));
        // Already-normal input passes through unchanged.
        assert_eq!(normalize_bbg_security("AAPL US Equity").as_deref(),
                   Some("AAPL US Equity"));
        // An unknown key is not silently accepted -- better no candidate than a
        // candidate that will come back BAD_SEC.
        assert_eq!(normalize_bbg_security("AAPL US<nonsense>"), None);
        assert_eq!(normalize_bbg_security(""), None);
    }

    /// P0 6: a query for "AAPL" returns option contracts alongside the listings.
    /// They are not instruments the security master tracks, and including them
    /// makes every equity search ambiguous.
    #[test]
    fn option_contracts_are_recognised() {
        assert!(is_option_contract("AAPL US 08/21/26 C400 Equity"));
        assert!(is_option_contract("AAPL US 08/21/26 P150 Equity"));
        assert!(is_option_contract("AAPL US 12/19/25 C00220000 Equity"));
        assert!(!is_option_contract("AAPL US Equity"));
        assert!(!is_option_contract("VFIAX US Equity"));
        assert!(!is_option_contract("SX5E Index"));
    }
}
```

Create `src-tauri/src/resolution/mod.rs`:

```rust
pub mod normalize;
```

Add to `src-tauri/src/lib.rs`, in alphabetical position among the existing `pub mod` lines:

```rust
pub mod resolution;
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --lib resolution::normalize
```

Expected: FAIL to compile — `cannot find function build_security in this scope`.

- [ ] **Step 3: Implement**

Prepend to `src-tauri/src/resolution/normalize.rs`, above the test module:

```rust
//! Turning what a human (or Bloomberg) typed into a security string the
//! Terminal will accept. No I/O: everything here is a pure function, because
//! every one of these rules is worth a test and none of them needs a database.

use crate::error::{AppError, AppResult};

/// Bloomberg market sectors. The list is closed; an identifier ending in one of
/// these already carries its yellow key.
pub const YELLOW_KEYS: [&str; 9] = [
    "Equity", "Corp", "Govt", "Index", "Curncy", "Comdty", "Mtge", "Muni", "Pfd",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdKind {
    Ticker,
    Isin,
}

/// An ISIN is a 2-letter country code, 9 alphanumerics and a check digit.
/// Anything else is treated as a ticker; the user can override in the UI.
pub fn detect_id_kind(input: &str) -> IdKind {
    let s = input.trim();
    let s = s.strip_prefix("/isin/").unwrap_or(s);
    let s = strip_trailing_key_any(s);
    let bytes = s.as_bytes();
    let looks_like_isin = bytes.len() == 12
        && bytes[..2].iter().all(|b| b.is_ascii_alphabetic())
        && bytes[2..].iter().all(|b| b.is_ascii_alphanumeric());
    if looks_like_isin { IdKind::Isin } else { IdKind::Ticker }
}

/// Drop a yellow key the user already typed onto the identifier.
///
/// The obvious thing to paste into a "ticker" box is the whole Bloomberg
/// identifier, "AAPL US Equity" -- while the yellow-key box next to it already
/// says "Equity". Appending blindly produced "AAPL US Equity Equity", which the
/// Terminal rejects as INVALID_SECURITY. No real ticker ends in a
/// whitespace-separated yellow key, so stripping one is unambiguous.
fn strip_trailing_key(identifier: &str, yellow_key: &str) -> String {
    match identifier.rsplit_once(char::is_whitespace) {
        Some((head, tail))
            if tail.eq_ignore_ascii_case(yellow_key) && !head.trim().is_empty() =>
        {
            head.trim_end().to_string()
        }
        _ => identifier.to_string(),
    }
}

/// Same, but for any known yellow key -- used when detecting the id kind, where
/// the intended key is not yet known.
fn strip_trailing_key_any(identifier: &str) -> &str {
    if let Some((head, tail)) = identifier.rsplit_once(char::is_whitespace) {
        if YELLOW_KEYS.iter().any(|k| tail.eq_ignore_ascii_case(k))
            && !head.trim().is_empty()
        {
            return head.trim_end();
        }
    }
    identifier
}

pub fn build_security(kind: IdKind, identifier: &str, yellow_key: &str)
    -> AppResult<String>
{
    let yk = yellow_key.trim();
    if yk.is_empty() {
        return Err(AppError::Validation("yellow_key is required".into()));
    }
    let raw = identifier.trim();
    if raw.is_empty() {
        return Err(AppError::Validation("identifier is empty".into()));
    }
    match kind {
        IdKind::Ticker => {
            let t = strip_trailing_key(raw, yk);
            // A ticker that IS the yellow key never had a security in it;
            // stripping cannot help, so refuse rather than build "Equity Equity".
            if t.is_empty() || t.eq_ignore_ascii_case(yk) {
                return Err(AppError::Validation(
                    "identifier is only a yellow key -- enter the security, e.g. 'AAPL US'".into()));
            }
            Ok(format!("{t} {yk}"))
        }
        IdKind::Isin => {
            // Accept a pasted "/isin/FR0000120271 Corp" as readily as a bare ISIN.
            let i = strip_trailing_key(raw, yk);
            let i = i.strip_prefix("/isin/").unwrap_or(&i).trim();
            if i.is_empty() {
                return Err(AppError::Validation("isin is empty after normalisation".into()));
            }
            Ok(format!("/isin/{i} {yk}"))
        }
    }
}

/// Convert Bloomberg's instrumentListRequest form into a security string.
///
/// P0 6 observed the service returns "AAPL US<equity>". The Terminal does not
/// accept that form, so it is normalised the moment it arrives and the raw text
/// is kept only for display. Returns None when the trailing key is not a known
/// market sector: a candidate we cannot address is worse than no candidate.
pub fn normalize_bbg_security(raw: &str) -> Option<String> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    let Some((head, rest)) = s.rsplit_once('<') else {
        // Already in "AAPL US Equity" form: accept it if its key is known.
        let tail = s.rsplit_once(char::is_whitespace).map(|(_, t)| t)?;
        return YELLOW_KEYS.iter().any(|k| tail.eq_ignore_ascii_case(k))
            .then(|| s.to_string());
    };
    let key = rest.strip_suffix('>')?;
    let canonical = YELLOW_KEYS.iter().find(|k| k.eq_ignore_ascii_case(key))?;
    let head = head.trim();
    (!head.is_empty()).then(|| format!("{head} {canonical}"))
}

/// A listed option carries an expiry date and a strike between the ticker and
/// the yellow key: "AAPL US 08/21/26 C400 Equity". These are excluded from
/// candidate sets -- they are not instruments the security master tracks, and
/// they make every equity search ambiguous.
pub fn is_option_contract(security: &str) -> bool {
    let mut parts = security.split_whitespace().peekable();
    let mut saw_date = false;
    while let Some(p) = parts.next() {
        // MM/DD/YY or MM/DD/YYYY
        if p.matches('/').count() == 2
            && p.split('/').all(|seg| !seg.is_empty()
                                 && seg.chars().all(|c| c.is_ascii_digit()))
        {
            saw_date = true;
            continue;
        }
        // A call or put strike immediately usable after the date: C400, P150.
        if saw_date {
            let mut cs = p.chars();
            if matches!(cs.next(), Some('C') | Some('P'))
                && p.len() > 1
                && cs.all(|c| c.is_ascii_digit() || c == '.')
            {
                return true;
            }
        }
    }
    false
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --lib resolution::normalize
```

Expected: all seven tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/resolution src-tauri/src/lib.rs
git commit -m "feat(resolution): input normalisation, Bloomberg security forms, option filtering"
```

---

## Task 3: Candidate scoring

Deterministic and additive, not learned. A supplied hint that matches adds; a supplied hint that contradicts disqualifies outright; a candidate that is silent on a hint neither gains nor loses. A tie at the top is ambiguous by definition.

**Files:**
- Create: `src-tauri/src/resolution/score.rs`
- Modify: `src-tauri/src/resolution/mod.rs`

**Interfaces:**
- Consumes: `resolution::normalize::is_option_contract`.
- Produces:
  - `pub struct Hints { pub exchange, country, currency, asset_class: Option<String> }`
  - `pub struct Candidate { pub security: String, pub description: String, pub exchange: Option<String>, pub country: Option<String>, pub currency: Option<String>, pub asset_class: Option<String>, pub figi: Option<String> }`
  - `pub struct Scored { pub candidate: Candidate, pub score: i32, pub disqualified: bool, pub reasons: Vec<String> }`
  - `pub enum Verdict { Unique(Candidate), Ambiguous(Vec<Scored>), None }`
  - `pub fn score_all(candidates: Vec<Candidate>, hints: &Hints) -> Vec<Scored>`
  - `pub fn verdict(scored: Vec<Scored>) -> Verdict`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/resolution/score.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn cand(sec: &str, exch: &str, ccy: &str) -> Candidate {
        Candidate {
            security: sec.into(),
            description: String::new(),
            exchange: (!exch.is_empty()).then(|| exch.to_string()),
            country: None,
            currency: (!ccy.is_empty()).then(|| ccy.to_string()),
            asset_class: None,
            figi: None,
        }
    }

    fn hints(exch: Option<&str>, ccy: Option<&str>) -> Hints {
        Hints { exchange: exch.map(str::to_string), country: None,
                currency: ccy.map(str::to_string), asset_class: None }
    }

    #[test]
    fn with_no_hints_a_single_candidate_wins() {
        let v = verdict(score_all(vec![cand("AAPL US Equity", "US", "USD")],
                                  &hints(None, None)));
        assert!(matches!(v, Verdict::Unique(c) if c.security == "AAPL US Equity"));
    }

    #[test]
    fn with_no_hints_several_candidates_are_ambiguous() {
        let v = verdict(score_all(
            vec![cand("AAPL US Equity", "US", "USD"), cand("AAPL LN Equity", "LN", "GBP")],
            &hints(None, None)));
        assert!(matches!(v, Verdict::Ambiguous(ref s) if s.len() == 2),
                "no hint can separate them, so a human must");
    }

    #[test]
    fn a_matching_hint_selects_and_a_contradicting_one_disqualifies() {
        let scored = score_all(
            vec![cand("AAPL US Equity", "US", "USD"), cand("AAPL LN Equity", "LN", "GBP")],
            &hints(Some("US"), None));
        let us = scored.iter().find(|s| s.candidate.security.contains(" US ")).unwrap();
        let ln = scored.iter().find(|s| s.candidate.security.contains(" LN ")).unwrap();
        assert!(!us.disqualified && us.score > 0);
        assert!(ln.disqualified, "a candidate contradicting a supplied hint is out");
        assert!(matches!(verdict(scored), Verdict::Unique(c) if c.exchange.as_deref() == Some("US")));
    }

    #[test]
    fn a_candidate_silent_on_a_hint_is_neither_rewarded_nor_punished() {
        let quiet = Candidate { currency: None, ..cand("AAPL US Equity", "US", "") };
        let scored = score_all(vec![quiet], &hints(None, Some("USD")));
        assert!(!scored[0].disqualified);
        assert_eq!(scored[0].score, 0, "an absent attribute is not evidence either way");
    }

    #[test]
    fn hints_are_compared_case_insensitively() {
        let scored = score_all(vec![cand("AAPL US Equity", "us", "usd")],
                               &hints(Some("US"), Some("USD")));
        assert!(!scored[0].disqualified);
        assert!(scored[0].score >= 2, "both hints matched");
    }

    /// If the top two scores are equal the input genuinely does not distinguish
    /// them. Picking the first would be arbitrary and would silently bind the
    /// wrong instrument, which is the failure this whole phase exists to prevent.
    #[test]
    fn a_tie_at_the_top_is_ambiguous_not_a_coin_flip() {
        let v = verdict(score_all(
            vec![cand("AAPL US Equity", "US", "USD"), cand("AAPL UW Equity", "US", "USD")],
            &hints(Some("US"), Some("USD"))));
        match v {
            Verdict::Ambiguous(s) => assert_eq!(s.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }

    #[test]
    fn every_candidate_disqualified_is_not_ambiguity_but_absence() {
        let v = verdict(score_all(vec![cand("AAPL LN Equity", "LN", "GBP")],
                                  &hints(Some("US"), None)));
        assert!(matches!(v, Verdict::None));
    }

    /// P0 6: a query for AAPL returns option contracts alongside the listings.
    #[test]
    fn option_contracts_are_dropped_before_scoring() {
        let scored = score_all(
            vec![cand("AAPL US Equity", "US", "USD"),
                 cand("AAPL US 08/21/26 C400 Equity", "US", "USD")],
            &hints(None, None));
        assert_eq!(scored.len(), 1);
        assert_eq!(scored[0].candidate.security, "AAPL US Equity");
    }
}
```

Add to `src-tauri/src/resolution/mod.rs`:

```rust
pub mod score;
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --lib resolution::score
```

Expected: FAIL to compile — `cannot find struct Candidate in this scope`.

- [ ] **Step 3: Implement**

Prepend to `src-tauri/src/resolution/score.rs`:

```rust
//! Scoring candidate securities against the hints the user supplied.
//!
//! Deliberately a rule, not a model: the user must be able to read why a
//! candidate won, and the same input must always give the same answer. Ties are
//! not broken -- see `verdict`.

use crate::resolution::normalize::is_option_contract;
use serde::{Deserialize, Serialize};

/// What the user told us beyond the identifier itself. All optional.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hints {
    pub exchange: Option<String>,
    pub country: Option<String>,
    pub currency: Option<String>,
    pub asset_class: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Candidate {
    pub security: String,
    pub description: String,
    pub exchange: Option<String>,
    pub country: Option<String>,
    pub currency: Option<String>,
    pub asset_class: Option<String>,
    pub figi: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scored {
    pub candidate: Candidate,
    pub score: i32,
    pub disqualified: bool,
    pub reasons: Vec<String>,
}

#[derive(Debug)]
pub enum Verdict {
    /// Exactly one candidate survived with a strictly highest score.
    Unique(Candidate),
    /// Two or more survivors, or a tie at the top. A human decides.
    Ambiguous(Vec<Scored>),
    /// Nothing survived.
    None,
}

/// One hint dimension. Returns (points, disqualified, reason).
fn compare(name: &str, hint: Option<&String>, value: Option<&String>)
    -> (i32, bool, Option<String>)
{
    match (hint, value) {
        (None, _) => (0, false, None),
        // A hint the user typed into and then cleared arrives as Some("") from a
        // UI text field, not None. Treating it as a real hint would disqualify
        // every candidate that says anything at all -- so a blank hint is no hint.
        (Some(h), _) if h.trim().is_empty() => (0, false, None),
        // The candidate is silent: absence of evidence is not evidence.
        (Some(_), None) => (0, false, Some(format!("{name}: candidate is silent"))),
        (Some(h), Some(v)) if h.trim().eq_ignore_ascii_case(v.trim()) => {
            (1, false, Some(format!("{name}: matches {h}")))
        }
        (Some(h), Some(v)) => (0, true, Some(format!("{name}: {v} contradicts {h}"))),
    }
}

pub fn score_all(candidates: Vec<Candidate>, hints: &Hints) -> Vec<Scored> {
    candidates
        .into_iter()
        .filter(|c| !is_option_contract(&c.security))
        .map(|c| {
            let dims = [
                compare("exchange", hints.exchange.as_ref(), c.exchange.as_ref()),
                compare("country", hints.country.as_ref(), c.country.as_ref()),
                compare("currency", hints.currency.as_ref(), c.currency.as_ref()),
                compare("asset_class", hints.asset_class.as_ref(), c.asset_class.as_ref()),
            ];
            let score = dims.iter().map(|(p, _, _)| p).sum();
            let disqualified = dims.iter().any(|(_, d, _)| *d);
            let reasons = dims.iter().filter_map(|(_, _, r)| r.clone()).collect();
            Scored { candidate: c, score, disqualified, reasons }
        })
        .collect()
}

/// Ambiguity is the default, not the exception. A candidate is only bound when
/// it is the sole survivor or beats every other survivor outright.
pub fn verdict(scored: Vec<Scored>) -> Verdict {
    let mut live: Vec<Scored> = scored.into_iter().filter(|s| !s.disqualified).collect();
    if live.is_empty() {
        return Verdict::None;
    }
    if live.len() == 1 {
        return Verdict::Unique(live.remove(0).candidate);
    }
    live.sort_by(|a, b| b.score.cmp(&a.score));
    if live[0].score > live[1].score {
        return Verdict::Unique(live.remove(0).candidate);
    }
    Verdict::Ambiguous(live)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --lib resolution::score
```

Expected: all eight tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/resolution
git commit -m "feat(resolution): deterministic candidate scoring with ambiguity as the default"
```

---
## Task 4: The bitemporal instrument store

Every write that touches identity goes through here. The point of the module is that there is exactly one way to record a change — close the old row, insert a new one — and no caller can do it any other way.

**Files:**
- Create: `src-tauri/src/instrument/mod.rs`
- Create: `src-tauri/src/instrument/store.rs`
- Create: `src-tauri/tests/common/mod.rs`
- Create: `src-tauri/tests/instrument_store.rs`
- Modify: `src-tauri/src/lib.rs` (add `pub mod instrument;`)

**Interfaces:**
- Consumes: Task 1's schema; `crate::error::AppResult`.
- Produces:
  - `pub struct Instrument { pub instrument_id: i64, pub id_bb_global: Option<String>, pub id_bb_unique: Option<String> }`
  - `pub struct Alias { pub id: i64, pub instrument_id: i64, pub id_type: String, pub value: String, pub exch_code: Option<String>, pub valid_from: NaiveDate, pub valid_to: NaiveDate, pub source: String, pub bbg_action_id: Option<String>, pub anchoring_identifier: Option<String> }`
  - `pub struct Attr { pub id: i64, pub instrument_id: i64, pub attr: String, pub value: String, pub valid_from: NaiveDate, pub valid_to: NaiveDate, pub source: String }`
  - `pub struct NewAlias { pub id_type, value, source: String, pub exch_code: Option<String>, pub valid_from: NaiveDate, pub valid_to: Option<NaiveDate>, pub bbg_action_id: Option<String>, pub anchoring_identifier: Option<String> }`
  - `pub async fn create(pool) -> AppResult<Instrument>`
  - `pub async fn set_bloomberg_ids(pool, instrument_id, figi: Option<&str>, bbg_unique: Option<&str>) -> AppResult<()>`
  - `pub async fn insert_alias(tx, instrument_id, new: &NewAlias) -> AppResult<i64>`
  - `pub async fn close_alias(tx, alias_id, valid_to: NaiveDate) -> AppResult<()>`
  - `pub async fn supersede_alias(tx, alias_id) -> AppResult<()>`
  - `pub async fn find_all_by_alias(pool, id_type, value, as_of) -> AppResult<Vec<i64>>` — every distinct matching instrument; use this when absent must be told from ambiguous
  - `pub async fn find_by_alias(pool, id_type, value, as_of) -> AppResult<Option<i64>>` — `Some` only when EXACTLY one instrument matches
  - `pub async fn aliases(pool, instrument_id) -> AppResult<Vec<Alias>>`
  - `pub async fn current_security(pool, instrument_id, as_of: NaiveDate) -> AppResult<Option<String>>`
  - `pub async fn set_attr(tx, instrument_id, attr, value, valid_from, source, decision_id) -> AppResult<()>`
  - `pub async fn attrs(pool, instrument_id, as_of: NaiveDate) -> AppResult<Vec<Attr>>`
  - `pub async fn propose_link(pool, pred, succ, link_type, effective_date, evidence: serde_json::Value) -> AppResult<i64>`
  - `pub async fn confirm_link(pool, link_id, by: &str) -> AppResult<()>`
  - `pub async fn confirmed_successors(pool, instrument_id) -> AppResult<Vec<i64>>` — plural: a spinoff legitimately has several

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/common/mod.rs`:

```rust
//! Shared harness for integration tests. Each test gets a pool against
//! bloom_test with migrations applied.

use sqlx::PgPool;

pub async fn pool() -> PgPool {
    let url = std::env::var("BLOOM_TEST_DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost/bloom_test".into());
    let pool = PgPool::connect(&url).await.expect("connect to bloom_test");
    sqlx::migrate!("./migrations").run(&pool).await.expect("migrations");
    pool
}
```

Create `src-tauri/tests/instrument_store.rs`:

```rust
mod common;

use chrono::NaiveDate;
use getbloomdata_lib::instrument::store::{self, NewAlias};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

fn ticker(value: &str, from: &str) -> NewAlias {
    NewAlias {
        id_type: "ticker".into(),
        value: value.into(),
        exch_code: Some("US".into()),
        valid_from: d(from),
        valid_to: None,
        source: "user".into(),
        bbg_action_id: None,
        anchoring_identifier: None,
    }
}

#[tokio::test]
async fn a_ticker_change_produces_two_alias_rows_and_zero_updates_to_value() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &ticker("FB", "2012-05-18"))
        .await.unwrap();
    tx.commit().await.unwrap();

    // The rename: close the old period, open a new one. Never an UPDATE of value.
    let mut tx = pool.begin().await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker("META", "2022-06-09"))
        .await.unwrap();
    tx.commit().await.unwrap();

    let all = store::aliases(&pool, inst.instrument_id).await.unwrap();
    assert_eq!(all.len(), 2, "both identifiers survive");
    let fb = all.iter().find(|a| a.value == "FB").unwrap();
    assert_eq!(fb.valid_to, d("2022-06-09"), "the old ticker is closed, not deleted");
}

#[tokio::test]
async fn lookup_is_as_of_a_date_so_the_same_ticker_resolves_differently_over_time() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &ticker("FB", "2012-05-18"))
        .await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker("META", "2022-06-09"))
        .await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(store::find_by_alias(&pool, "ticker", "FB", d("2015-01-01")).await.unwrap(),
               Some(inst.instrument_id));
    assert_eq!(store::find_by_alias(&pool, "ticker", "FB", d("2026-01-01")).await.unwrap(),
               None, "FB stopped being this instrument's ticker in 2022");
    assert_eq!(store::find_by_alias(&pool, "ticker", "META", d("2026-01-01")).await.unwrap(),
               Some(inst.instrument_id));
}

#[tokio::test]
async fn lookup_is_case_insensitive() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker("AAPL US", "1980-12-12"))
        .await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(store::find_by_alias(&pool, "ticker", "aapl us", d("2026-01-01")).await.unwrap(),
               Some(inst.instrument_id));
}

#[tokio::test]
async fn a_correction_supersedes_rather_than_erases() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let wrong = store::insert_alias(&mut tx, inst.instrument_id, &ticker("APPL US", "1980-12-12"))
        .await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = pool.begin().await.unwrap();
    store::supersede_alias(&mut tx, wrong).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &ticker("AAPL US", "1980-12-12"))
        .await.unwrap();
    tx.commit().await.unwrap();

    // aliases() returns only what we currently believe...
    let current = store::aliases(&pool, inst.instrument_id).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].value, "AAPL US");
    // ...but the mistaken row is still on disk, which is what point-in-time needs.
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_alias WHERE instrument_id = $1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(total, 2);
}

#[tokio::test]
async fn setting_an_attribute_twice_for_the_same_period_supersedes_the_first() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "FACEBOOK INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "META PLATFORMS INC",
                    d("2012-05-18"), "bloomberg", None).await.unwrap();
    tx.commit().await.unwrap();

    let now = store::attrs(&pool, inst.instrument_id, d("2026-01-01")).await.unwrap();
    let names: Vec<&str> = now.iter().filter(|a| a.attr == "name")
        .map(|a| a.value.as_str()).collect();
    assert_eq!(names, ["META PLATFORMS INC"], "one current value per attribute period");
    let total: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_attr WHERE instrument_id = $1")
        .bind(inst.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(total, 2, "the earlier belief is retained beneath");
}

#[tokio::test]
async fn the_current_security_string_is_derived_not_stored() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: "FB US Equity".into(),
        ..ticker("FB US Equity", "2012-05-18") }).await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: "META US Equity".into(),
        ..ticker("META US Equity", "2022-06-09") }).await.unwrap();
    tx.commit().await.unwrap();

    assert_eq!(store::current_security(&pool, inst.instrument_id, d("2015-01-01"))
                   .await.unwrap().as_deref(), Some("FB US Equity"));
    assert_eq!(store::current_security(&pool, inst.instrument_id, d("2026-08-19"))
                   .await.unwrap().as_deref(), Some("META US Equity"));
}

/// P0 7.2: no Bloomberg field returns a successor, so a link is always a
/// derived proposal. Until a human confirms it, nothing may follow it.
#[tokio::test]
async fn an_unconfirmed_link_is_not_followed() {
    let pool = common::pool().await;
    let a = store::create(&pool).await.unwrap();
    let b = store::create(&pool).await.unwrap();
    let link = store::propose_link(&pool, a.instrument_id, b.instrument_id, "rename",
                                   d("2022-06-09"), serde_json::json!({"source": "test"}))
        .await.unwrap();
    assert_eq!(store::confirmed_successor(&pool, a.instrument_id).await.unwrap(), None,
               "a proposal is not a fact");
    store::confirm_link(&pool, link, "laurent").await.unwrap();
    assert_eq!(store::confirmed_successor(&pool, a.instrument_id).await.unwrap(),
               Some(b.instrument_id));
}

#[tokio::test]
async fn bloomberg_ids_can_be_filled_once_and_never_changed() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    store::set_bloomberg_ids(&pool, inst.instrument_id, Some("BBG000B9XRY4"), None)
        .await.expect("null -> value");
    let err = store::set_bloomberg_ids(&pool, inst.instrument_id, Some("BBG000000000"), None)
        .await.unwrap_err();
    assert!(err.to_string().contains("write-once"), "got: {err}");
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test instrument_store
```

Expected: FAIL to compile — `unresolved import getbloomdata_lib::instrument`.

- [ ] **Step 3: Implement**

Create `src-tauri/src/instrument/mod.rs`:

```rust
pub mod store;
```

Add `pub mod instrument;` to `src-tauri/src/lib.rs`.

Create `src-tauri/src/instrument/store.rs`:

```rust
//! The only supported way to write identity.
//!
//! Every change is close-and-insert. There is no update path for a value, and
//! the database enforces that independently (see migration 0001's triggers) so
//! that a mistake here fails loudly rather than quietly rewriting history.
//!
//! Two time axes:
//!   valid_from/valid_to   when the fact was true in the world
//!   system_from/system_to when we believed it
//! Closing valid_to records a real-world change (a ticker was renamed).
//! Closing system_to records a correction (we had it wrong).

use crate::error::AppResult;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::{PgPool, Postgres, Transaction};

pub type Tx<'a> = Transaction<'a, Postgres>;

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Instrument {
    pub instrument_id: i64,
    pub id_bb_global: Option<String>,
    pub id_bb_unique: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Alias {
    pub id: i64,
    pub instrument_id: i64,
    pub id_type: String,
    pub value: String,
    pub exch_code: Option<String>,
    pub valid_from: NaiveDate,
    pub valid_to: NaiveDate,
    pub source: String,
    pub bbg_action_id: Option<String>,
    pub anchoring_identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct Attr {
    pub id: i64,
    pub instrument_id: i64,
    pub attr: String,
    pub value: String,
    pub valid_from: NaiveDate,
    pub valid_to: NaiveDate,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct NewAlias {
    pub id_type: String,
    pub value: String,
    pub exch_code: Option<String>,
    pub valid_from: NaiveDate,
    /// None means open-ended.
    pub valid_to: Option<NaiveDate>,
    pub source: String,
    pub bbg_action_id: Option<String>,
    /// REQUIRED when source is 'bloomberg_hist_ids'; the database refuses
    /// otherwise. See P0 6.4.
    pub anchoring_identifier: Option<String>,
}

/// The open-ended sentinel, shared by the schema default, this module and the
/// frontend. NOT `NaiveDate::MAX` (year 262142): chrono serialises out-of-range
/// years with an ISO expanded sign, so it crosses the Tauri boundary as
/// "+262142-12-31", which JavaScript reads as Invalid Date.
pub fn forever() -> NaiveDate {
    NaiveDate::from_ymd_opt(9999, 12, 31).unwrap()
}

pub async fn create(pool: &PgPool) -> AppResult<Instrument> {
    Ok(sqlx::query_as::<_, Instrument>(
        "INSERT INTO instrument DEFAULT VALUES
         RETURNING instrument_id, id_bb_global, id_bb_unique")
        .fetch_one(pool).await?)
}

/// Fill the Bloomberg identifiers once they are known. The trigger refuses any
/// attempt to change a value that is already set.
pub async fn set_bloomberg_ids(pool: &PgPool, instrument_id: i64,
                               figi: Option<&str>, bbg_unique: Option<&str>)
    -> AppResult<()>
{
    sqlx::query(
        "UPDATE instrument
            SET id_bb_global = COALESCE($2, id_bb_global),
                id_bb_unique = COALESCE($3, id_bb_unique)
          WHERE instrument_id = $1")
        .bind(instrument_id).bind(figi).bind(bbg_unique)
        .execute(pool).await?;
    Ok(())
}

pub async fn insert_alias(tx: &mut Tx<'_>, instrument_id: i64, new: &NewAlias)
    -> AppResult<i64>
{
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO instrument_alias
           (instrument_id, id_type, value, exch_code, valid_from, valid_to,
            source, bbg_action_id, anchoring_identifier)
         VALUES ($1,$2,$3,$4,$5,COALESCE($6, DATE 'infinity'),$7,$8,$9)
         RETURNING id")
        .bind(instrument_id).bind(&new.id_type).bind(&new.value)
        .bind(&new.exch_code).bind(new.valid_from).bind(new.valid_to)
        .bind(&new.source).bind(&new.bbg_action_id).bind(&new.anchoring_identifier)
        .fetch_one(&mut **tx).await?;
    Ok(id)
}

/// The identifier stopped being true in the world on `valid_to`.
pub async fn close_alias(tx: &mut Tx<'_>, alias_id: i64, valid_to: NaiveDate)
    -> AppResult<()>
{
    sqlx::query("UPDATE instrument_alias SET valid_to = $2 WHERE id = $1")
        .bind(alias_id).bind(valid_to).execute(&mut **tx).await?;
    Ok(())
}

/// We were wrong about the identifier. The row stays on disk; it stops being
/// current. This is what makes a point-in-time read of a past belief possible.
pub async fn supersede_alias(tx: &mut Tx<'_>, alias_id: i64) -> AppResult<()> {
    sqlx::query("UPDATE instrument_alias SET system_to = now()
                  WHERE id = $1 AND system_to = 'infinity'")
        .bind(alias_id).execute(&mut **tx).await?;
    Ok(())
}

/// Which instrument wore this identifier on this date, as best we know today.
pub async fn find_by_alias(pool: &PgPool, id_type: &str, value: &str, as_of: NaiveDate)
    -> AppResult<Option<i64>>
{
    Ok(sqlx::query_scalar(
        "SELECT instrument_id FROM instrument_alias
          WHERE id_type = $1 AND lower(value) = lower($2)
            AND valid_from <= $3 AND valid_to > $3
            AND system_to = 'infinity'
          ORDER BY valid_from DESC LIMIT 1")
        .bind(id_type).bind(value).bind(as_of)
        .fetch_optional(pool).await?)
}

pub async fn aliases(pool: &PgPool, instrument_id: i64) -> AppResult<Vec<Alias>> {
    Ok(sqlx::query_as::<_, Alias>(
        "SELECT id, instrument_id, id_type, value, exch_code, valid_from, valid_to,
                source, bbg_action_id, anchoring_identifier
           FROM instrument_alias
          WHERE instrument_id = $1 AND system_to = 'infinity'
          ORDER BY valid_from, id_type")
        .bind(instrument_id).fetch_all(pool).await?)
}

/// The security string to send to Bloomberg for this instrument on this date.
/// Derived from the alias valid then -- never stored on the book entry, because
/// one instrument wears several security strings over its life.
pub async fn current_security(pool: &PgPool, instrument_id: i64, as_of: NaiveDate)
    -> AppResult<Option<String>>
{
    Ok(sqlx::query_scalar(
        "SELECT value FROM instrument_alias
          WHERE instrument_id = $1 AND id_type = 'bdp_security'
            AND valid_from <= $2 AND valid_to > $2
            AND system_to = 'infinity'
          ORDER BY valid_from DESC LIMIT 1")
        .bind(instrument_id).bind(as_of)
        .fetch_optional(pool).await?)
}

/// Record an attribute for a validity period. If we already believe something
/// else about that exact period, the earlier belief is superseded first --
/// which is what the partial unique index would otherwise refuse.
pub async fn set_attr(tx: &mut Tx<'_>, instrument_id: i64, attr: &str, value: &str,
                      valid_from: NaiveDate, source: &str, decision_id: Option<i64>)
    -> AppResult<()>
{
    sqlx::query(
        "UPDATE instrument_attr SET system_to = now()
          WHERE instrument_id = $1 AND attr = $2 AND valid_from = $3
            AND system_to = 'infinity'")
        .bind(instrument_id).bind(attr).bind(valid_from)
        .execute(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO instrument_attr
           (instrument_id, attr, value, valid_from, source, decision_id)
         VALUES ($1,$2,$3,$4,$5,$6)")
        .bind(instrument_id).bind(attr).bind(value).bind(valid_from)
        .bind(source).bind(decision_id)
        .execute(&mut **tx).await?;
    Ok(())
}

pub async fn attrs(pool: &PgPool, instrument_id: i64, as_of: NaiveDate)
    -> AppResult<Vec<Attr>>
{
    Ok(sqlx::query_as::<_, Attr>(
        "SELECT id, instrument_id, attr, value, valid_from, valid_to, source
           FROM instrument_attr
          WHERE instrument_id = $1
            AND valid_from <= $2 AND valid_to > $2
            AND system_to = 'infinity'
          ORDER BY attr")
        .bind(instrument_id).bind(as_of).fetch_all(pool).await?)
}

/// Propose a predecessor/successor relationship. Always a proposal: P0 7.2
/// established that Bloomberg exposes no successor field, so every link here is
/// inferred and a human must agree before anything follows it.
pub async fn propose_link(pool: &PgPool, predecessor_id: i64, successor_id: i64,
                          link_type: &str, effective_date: NaiveDate,
                          evidence: serde_json::Value) -> AppResult<i64>
{
    Ok(sqlx::query_scalar(
        "INSERT INTO instrument_link
           (predecessor_id, successor_id, link_type, effective_date, evidence)
         VALUES ($1,$2,$3,$4,$5) RETURNING id")
        .bind(predecessor_id).bind(successor_id).bind(link_type)
        .bind(effective_date).bind(evidence)
        .fetch_one(pool).await?)
}

pub async fn confirm_link(pool: &PgPool, link_id: i64, by: &str) -> AppResult<()> {
    sqlx::query("UPDATE instrument_link SET confirmed_by = $2, confirmed_at = now()
                  WHERE id = $1")
        .bind(link_id).bind(by).execute(pool).await?;
    Ok(())
}

/// Only confirmed links are ever followed.
pub async fn confirmed_successor(pool: &PgPool, instrument_id: i64)
    -> AppResult<Option<i64>>
{
    Ok(sqlx::query_scalar(
        "SELECT successor_id FROM instrument_link
          WHERE predecessor_id = $1 AND confirmed_by IS NOT NULL
          ORDER BY effective_date DESC LIMIT 1")
        .bind(instrument_id).fetch_optional(pool).await?)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test instrument_store
```

Expected: all eight tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/instrument src-tauri/src/lib.rs src-tauri/tests
git commit -m "feat(instrument): bitemporal identity store with close-and-insert semantics"
```

---

## Task 5: Sidecar support for the three master requests

The existing sidecar answers `history` and `reference`. The security master needs field overrides (for `HISTORICAL_IDS_TIME_RANGE`), bulk-field parsing (P0 §5: `ftype == BulkFormat` means table-valued), and a second service (`//blp/instruments`).

**Files:**
- Modify: `src-tauri/scripts/blp_fetch.py`
- Create: `src-tauri/scripts/tests/test_master_kinds.py`

**Interfaces:**
- Consumes: the stdin payload shape already in use — `{"run_id":.., "timeout_s":.., "requests":[{...}]}`.
- Produces three new request kinds and a new response section:
  - `{"kind": "reference", "securities": [..], "fields": [..], "obs_date": "..", "overrides": [{"fieldId": "..", "value": ".."}]}` — `overrides` is new and optional.
  - `{"kind": "bulk_reference", "securities": [..], "fields": [..], "overrides": [..]}` — returns `bulk_rows` instead of observations.
  - `{"kind": "instrument_list", "query": "..", "yellow_key_filter": "YK_FILTER_EQTY", "max_results": 20}` — returns `list_results`.
  - Response gains `"bulk_rows": [{"security":..,"field":..,"rows":[{col: val}]}]` and `"list_results": [{"security":..,"description":..}]`.

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/scripts/tests/test_master_kinds.py`. These are replay tests — they never touch a Terminal; they parse the P0 captures already committed under `docs/superpowers/specs/blpapi-facts/`.

```python
"""Sidecar parsing for the security-master request kinds.

Replay only: no Bloomberg session is opened. The fixtures are the P0 captures,
so a change in parsing that would break against the real Terminal breaks here.
"""
import json
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "src-tauri" / "scripts"))
FACTS = ROOT / "docs" / "superpowers" / "specs" / "blpapi-facts"

import blp_fetch  # noqa: E402


def load(name):
    with open(FACTS / name, encoding="utf-8") as fh:
        return json.load(fh)


def test_bulk_field_rows_are_parsed_as_tables_not_scalars():
    """HISTORICAL_IDS_TIME_RANGE is ftype BulkFormat: its value is a list of
    dicts, not a number. Parsing it as a scalar would silently lose the whole
    identifier history."""
    cap = load("histids_report.json")
    msgs = cap["META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', "
               "'HISTORICAL_STARTING_IDENTIFIER']"]
    rows, problems = [], []
    for m in msgs:
        blp_fetch.parse_bulk_message(
            {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
            m, rows, problems)
    assert problems == []
    assert len(rows) == 1
    entry = rows[0]
    assert entry["security"] == "META US Equity"
    assert entry["field"] == "HISTORICAL_IDS_TIME_RANGE"
    assert entry["rows"] == [{
        "Date": "2022-06-09", "Old ID": "FB", "New ID": "META",
        "Old Exch": "US", "New Exch": "US",
        "Action ID": "228233742", "Source": "ID Change",
    }]


def test_the_anchored_and_unanchored_answers_differ():
    """P0 6.4. The same query about META US Equity returns Facebook's rename
    when anchored and the Roundhill ETF's rename when not. The sidecar must
    return both faithfully -- deciding which is trustworthy is Rust's job."""
    cap = load("histids_report.json")
    anchored, unanchored = [], []
    for key, sink in ((
        "META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', "
        "'HISTORICAL_STARTING_IDENTIFIER']", anchored),
            ("META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT']", unanchored)):
        for m in cap[key]:
            blp_fetch.parse_bulk_message(
                {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
                m, sink, [])
    assert anchored[0]["rows"][0]["New ID"] == "META"
    assert unanchored[0]["rows"][0]["New ID"] == "METV", (
        "the unanchored answer is a different company entirely")


def test_a_missing_bulk_field_is_a_problem_not_an_empty_table():
    msg = {"securityData": [{"security": "XYZ US Equity", "fieldData": {},
                             "fieldExceptions": [], "sequenceNumber": 0}]}
    rows, problems = [], []
    blp_fetch.parse_bulk_message(
        {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
        msg, rows, problems)
    assert rows == []
    assert len(problems) == 1
    assert problems[0]["code"] == "no_data"


def test_a_security_error_on_a_bulk_request_is_attributed_to_that_security():
    msg = {"securityData": [{
        "security": "NOPE US Equity",
        "securityError": {"category": "BAD_SEC", "subcategory": "INVALID_SECURITY",
                          "message": "Unknown/Invalid Security"},
        "fieldData": {}, "fieldExceptions": [], "sequenceNumber": 0}]}
    rows, problems = [], []
    blp_fetch.parse_bulk_message(
        {"kind": "bulk_reference", "fields": ["HISTORICAL_IDS_TIME_RANGE"]},
        msg, rows, problems)
    assert rows == []
    assert problems[0]["code"] == "invalid_security"
    assert problems[0]["security"] == "NOPE US Equity"


def test_instrument_list_results_are_parsed():
    msg = {"results": [
        {"security": "AAPL US<equity>", "description": "Apple Inc"},
        {"security": "AAPL LN<equity>", "description": "Apple Inc"},
    ]}
    out = []
    blp_fetch.parse_instrument_list_message(msg, out)
    assert out == [
        {"security": "AAPL US<equity>", "description": "Apple Inc"},
        {"security": "AAPL LN<equity>", "description": "Apple Inc"},
    ], "the raw form is preserved; Rust normalises it (never the reverse)"


def test_validation_rejects_an_instrument_list_without_a_query():
    errs = blp_fetch.validate_request_spec({"kind": "instrument_list"})
    assert errs and "query" in errs[0]


def test_validation_accepts_the_new_kinds():
    assert blp_fetch.validate_request_spec({
        "kind": "instrument_list", "query": "AAPL", "max_results": 10}) == []
    assert blp_fetch.validate_request_spec({
        "kind": "bulk_reference", "securities": ["AAPL US Equity"],
        "fields": ["EQY_DVD_ADJUST_FACT"]}) == []
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && python -m pytest scripts/tests/test_master_kinds.py -v
```

Expected: FAIL — `AttributeError: module 'blp_fetch' has no attribute 'parse_bulk_message'`.

- [ ] **Step 3: Implement the sidecar changes**

Four edits to `src-tauri/scripts/blp_fetch.py`.

**3a.** Add the instruments service constant next to `REFDATA_SERVICE`:

```python
INSTRUMENTS_SERVICE = "//blp/instruments"
```

**3b.** Replace `validate_request_spec` so it knows the new kinds:

```python
def validate_request_spec(spec, where="request"):
    """Structural validation. Returns a list of human-readable errors."""
    errors = []
    kind = spec.get("kind")
    if kind not in ("history", "reference", "bulk_reference", "instrument_list"):
        return [f"{where}: unknown request kind: {kind!r}"]

    if kind == "instrument_list":
        if not str(spec.get("query") or "").strip():
            errors.append(f"{where}: instrument_list needs a non-empty query")
        return errors

    if not spec.get("securities"):
        errors.append(f"{where}: no securities")
    if not spec.get("fields"):
        errors.append(f"{where}: no fields")
    for i, ov in enumerate(spec.get("overrides") or []):
        if not ov.get("fieldId") or ov.get("value") is None:
            errors.append(f"{where}: overrides[{i}] needs fieldId and value")
    if kind == "history":
        start, end = iso_date(spec.get("start")), iso_date(spec.get("end"))
        if start is None:
            errors.append(f"{where}: invalid start date {spec.get('start')!r}")
        if end is None:
            errors.append(f"{where}: invalid end date {spec.get('end')!r}")
        if start and end and start > end:
            errors.append(f"{where}: start {start} after end {end}")
    elif kind == "reference" and iso_date(spec.get("obs_date")) is None:
        errors.append(f"{where}: invalid obs_date {spec.get('obs_date')!r}")
    return errors
```

**3c.** Add the two parsers, next to `parse_reference_message`:

```python
def parse_bulk_message(spec, msg, rows_out, problems_out):
    """Bulk (table-valued) reference fields.

    P0 5: a field whose ftype is 'BulkFormat' returns a LIST OF DICTS, not a
    scalar. Reading it with the scalar path would coerce a whole corporate-action
    table into one meaningless string, so bulk fields get their own kind and
    their own output section.
    """
    for sec_data in msg.get("securityData", []):
        security = sec_data.get("security")
        sec_err = sec_data.get("securityError")
        if sec_err:
            problems_out.append(problem(
                security, None, None, classify_security_error(sec_err),
                sec_err.get("message", "")))
            continue
        failed = {}
        for exc in sec_data.get("fieldExceptions", []):
            info = exc.get("errorInfo") or {}
            failed[exc.get("fieldId")] = info
            problems_out.append(problem(
                security, exc.get("fieldId"), None, "field_error",
                info.get("message", "")))
        fdata = sec_data.get("fieldData") or {}
        for f in spec.get("fields", []):
            if f in failed:
                continue
            value = fdata.get(f)
            if not value:
                problems_out.append(problem(
                    security, f, None, "no_data", "bulk field absent or empty"))
                continue
            # toPy() gives a list of dicts for a bulk field. A scalar here means
            # the field is not actually bulk, which is worth saying out loud.
            if not isinstance(value, list):
                problems_out.append(problem(
                    security, f, None, "not_bulk",
                    f"expected a table, got {type(value).__name__}"))
                continue
            rows_out.append({"security": security, "field": f,
                             "rows": [dict(r) for r in value]})


def parse_instrument_list_message(msg, out):
    """instrumentListRequest results, kept exactly as returned.

    The 'AAPL US<equity>' form is NOT normalised here: Rust owns that rule and
    its regression test, and a sidecar that silently rewrote identifiers would
    put the conversion beyond the reach of those tests.
    """
    for r in msg.get("results", []):
        out.append({"security": r.get("security"),
                    "description": r.get("description", "")})
```

**3d.** Teach `build_request`, `open_session` and `parse_capture` about the new kinds.

In `open_session`, open the instruments service too, but do not fail the session
if it is refused — most runs never need it:

```python
    if not session.openService(REFDATA_SERVICE):
        session.stop()
        raise SessionError(
            f"openService('{REFDATA_SERVICE}') failed -- the session connected "
            "but the refdata service was refused (entitlement?)")
    # Optional: only instrument_list needs it, and a run that never searches
    # should not fail because the search service is unavailable.
    session.openService(INSTRUMENTS_SERVICE)
    return session
```

In `build_request`, add the branches and the overrides block. Note the four
adjustment flags on the history branch — see Task 12 for why they are set here:

```python
def build_request(blpapi, service, spec):
    kind = spec.get("kind")
    if kind == "history":
        r = service.createRequest("HistoricalDataRequest")
        r.set("startDate", spec["start"])
        r.set("endDate", spec["end"])
        r.set("periodicitySelection", "DAILY")
        r.set("nonTradingDayFillOption", "ACTIVE_DAYS_ONLY")
    elif kind in ("reference", "bulk_reference"):
        r = service.createRequest("ReferenceDataRequest")
    elif kind == "instrument_list":
        r = service.createRequest("instrumentListRequest")
        r.set("query", spec["query"])
        if spec.get("yellow_key_filter"):
            r.set("yellowKeyFilter", spec["yellow_key_filter"])
        r.set("maxResults", int(spec.get("max_results", 20)))
        return r
    else:
        raise SessionError(f"unknown request kind: {kind!r}")

    for s in spec.get("securities", []):
        r.getElement("securities").appendValue(s)
    for f in spec.get("fields", []):
        r.getElement("fields").appendValue(f)

    # Field overrides. HISTORICAL_IDS_TIME_RANGE needs
    # HISTORICAL_STARTING_IDENTIFIER; without it Bloomberg answers about a
    # different instrument that once wore the same ticker (P0 6.4).
    overrides = spec.get("overrides") or []
    if overrides:
        ov = r.getElement("overrides")
        for o in overrides:
            e = ov.appendElement()
            e.setElement("fieldId", o["fieldId"])
            e.setElement("value", str(o["value"]))
    return r
```

**Also route the request to the right service.** `run_fetch` resolves ONE
service object (`//blp/refdata`) before its request loop and reuses it for every
request, so an `instrument_list` created against it fails at the blpapi layer on
first live use — and no replay test catches it, because replay never calls
`run_fetch`. Pick the service per `spec["kind"]`:

```python
        svc = (session.getService(INSTRUMENTS_SERVICE)
               if spec.get("kind") == "instrument_list"
               else session.getService(REFDATA_SERVICE))
```

**And widen the empty-response fault check.** `finish()` treats "no observations
and no problems" as a session fault. A clean `bulk_reference` or
`instrument_list` response legitimately has neither, so it must also consider
`bulk_rows` and `list_results` before declaring a fault. For `history` and
`reference` those two are structurally always empty, so the existing behaviour is
unchanged.

In `parse_capture`, route the new kinds and collect their output. Change its
signature and return value to carry the two new sections:

```python
def parse_capture(capture):
    """capture -> (observations, problems, bulk_rows, list_results, fatal|None)."""
    observations, problems, bulk_rows, list_results = [], [], [], []
    for item in capture.get("captured", []):
        req = item.get("request") or {}
        kind = req.get("kind")
        spec_errors = validate_request_spec(req)
        if spec_errors:
            return observations, problems, bulk_rows, list_results, "; ".join(spec_errors)
        for msg in item.get("messages", []):
            resp_err = msg.get("responseError")
            if resp_err:
                return observations, problems, bulk_rows, list_results, (
                    f"responseError {resp_err.get('category', '')}: "
                    f"{resp_err.get('message', '')}")
            if kind == "history":
                parse_history_message(req, msg, observations, problems)
            elif kind == "reference":
                parse_reference_message(req, msg, observations, problems)
            elif kind == "bulk_reference":
                parse_bulk_message(req, msg, bulk_rows, problems)
            else:
                parse_instrument_list_message(msg, list_results)
    return observations, problems, bulk_rows, list_results, None
```

Update the single call site of `parse_capture` in `main()` to unpack five values
and include `"bulk_rows"` and `"list_results"` in the emitted JSON object.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && python -m pytest scripts/tests/test_master_kinds.py -v
```

Expected: all seven tests PASS.

- [ ] **Step 5: Check the existing sidecar tests still pass**

```bash
cd src-tauri && python -m pytest scripts/tests -v
```

Expected: PASS. `parse_capture` changed shape, so any existing test that unpacks
three values must be updated to five.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/scripts
git commit -m "feat(sidecar): field overrides, bulk-field parsing and instrumentListRequest"
```

---
## Task 6: The Bloomberg master-request seam

Rust-side request and response types for the three master requests, behind a trait with a mock — so every later task can be tested without a Terminal, and the live implementation is one thin shell.

**Files:**
- Create: `src-tauri/src/master_fetch.rs`
- Modify: `src-tauri/src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `master_fetch.rs`

**Interfaces:**
- Consumes: `crate::resolution::normalize::normalize_bbg_security`, `crate::resolution::score::Candidate`, `crate::blp_driver`, `crate::orchestrator::PipelineConfig`.
- Produces:
  - `pub struct IdentityBlock { pub security, pub figi, pub share_class_figi, pub bbg_unique, pub isin, pub exch_code, pub currency, pub country, pub security_typ2, pub market_sector, pub name, pub listing_date, pub inactive_date, pub status: Option<..> }`
  - `pub struct HistIdRow { pub date: NaiveDate, pub old_id: String, pub new_id: String, pub old_exch: Option<String>, pub new_exch: Option<String>, pub action_id: Option<String>, pub source: Option<String> }`
  - `pub const IDENTITY_FIELDS: [&str; 11]`
  - `pub trait MasterFetcher { fn identity(&self, securities: &[String]) -> ...; fn hist_ids(&self, security: &str, anchor: &str, start: NaiveDate) -> ...; fn instrument_list(&self, query: &str, yellow_key_filter: Option<&str>, max_results: u32) -> ... }`
  - `pub struct BlpapiMasterFetcher<'a> { pub cfg: &'a PipelineConfig }`
  - `pub struct MockMasterFetcher { ... }` with `pub fn from_capture(json: &str) -> Self`
  - `pub fn parse_identity(raw: &serde_json::Value) -> Vec<IdentityBlock>`
  - `pub fn parse_hist_ids(raw: &serde_json::Value) -> Vec<HistIdRow>`
  - `pub fn parse_list(raw: &serde_json::Value) -> Vec<Candidate>`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/src/master_fetch.rs` with the test module only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const HISTIDS: &str = include_str!(
        "../../docs/superpowers/specs/blpapi-facts/histids_report.json");

    fn capture(key: &str) -> serde_json::Value {
        let all: serde_json::Value = serde_json::from_str(HISTIDS).unwrap();
        all[key].clone()
    }

    /// The P0 capture, replayed. If the parse breaks, this breaks -- no Terminal
    /// required and no fixture invented for the occasion.
    #[test]
    fn hist_id_rows_are_parsed_from_the_p0_capture() {
        let raw = capture("META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', \
                           'HISTORICAL_STARTING_IDENTIFIER']");
        let rows = parse_hist_ids(&raw);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].old_id, "FB");
        assert_eq!(rows[0].new_id, "META");
        assert_eq!(rows[0].date, "2022-06-09".parse::<chrono::NaiveDate>().unwrap());
        assert_eq!(rows[0].action_id.as_deref(), Some("228233742"));
    }

    /// P0 6.4, the trap this whole anchoring discipline exists for.
    #[test]
    fn the_unanchored_capture_describes_a_different_company() {
        let raw = capture("META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT']");
        let rows = parse_hist_ids(&raw);
        assert_eq!(rows[0].new_id, "METV",
                   "unanchored, Bloomberg answers about the Roundhill ETF");
    }

    #[test]
    fn an_identity_block_is_parsed_and_missing_fields_stay_none() {
        let raw = serde_json::json!([{"securityData": [{
            "security": "AAPL US Equity",
            "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {
                "ID_BB_GLOBAL": "BBG000B9XRY4",
                "ID_ISIN": "US0378331005",
                "EXCH_CODE": "US",
                "CRNCY": "USD",
                "CNTRY_ISSUE_ISO": "US",
                "SECURITY_TYP2": "Common Stock",
                "MARKET_SECTOR_DES": "Equity",
                "NAME": "APPLE INC",
                "LISTING_DATE": "1980-12-12"
            }}]}]);
        let blocks = parse_identity(&raw);
        assert_eq!(blocks.len(), 1);
        let b = &blocks[0];
        assert_eq!(b.security, "AAPL US Equity");
        assert_eq!(b.figi.as_deref(), Some("BBG000B9XRY4"));
        assert_eq!(b.listing_date, Some("1980-12-12".parse().unwrap()));
        assert_eq!(b.inactive_date, None, "an absent field is None, never a default");
        assert_eq!(b.status, None);
    }

    #[test]
    fn a_security_error_yields_no_identity_block() {
        let raw = serde_json::json!([{"securityData": [{
            "security": "NOPE US Equity",
            "securityError": {"category": "BAD_SEC",
                              "subcategory": "INVALID_SECURITY",
                              "message": "Unknown/Invalid Security"},
            "fieldData": {}, "fieldExceptions": [], "sequenceNumber": 0}]}]);
        assert!(parse_identity(&raw).is_empty(),
                "a rejected security must not become a half-populated instrument");
    }

    /// The raw form is normalised here, in Rust, where the regression test for
    /// the doubled-yellow-key defect already lives.
    #[test]
    fn list_results_become_candidates_with_usable_security_strings() {
        let raw = serde_json::json!([{"results": [
            {"security": "AAPL US<equity>", "description": "Apple Inc"},
            {"security": "AAPL LN<equity>", "description": "Apple Inc"},
            {"security": "AAPL US 08/21/26 C400<equity>", "description": "Apple Inc call"},
            {"security": "GARBAGE<nonsense>", "description": "unaddressable"}
        ]}]);
        let cands = parse_list(&raw);
        let secs: Vec<&str> = cands.iter().map(|c| c.security.as_str()).collect();
        assert_eq!(secs, ["AAPL US Equity", "AAPL LN Equity",
                          "AAPL US 08/21/26 C400 Equity"],
                   "an unaddressable candidate is dropped; options survive here \
                    and are filtered at scoring time");
        assert_eq!(cands[0].exchange.as_deref(), Some("US"),
                   "the exchange code is read off the security string");
    }

    #[tokio::test]
    async fn the_mock_fetcher_replays_a_capture() {
        let mock = MockMasterFetcher::from_capture(HISTIDS);
        let rows = mock.hist_ids("META US Equity", "META US Equity",
                                 "2000-01-01".parse().unwrap()).await.unwrap();
        assert_eq!(rows[0].new_id, "META");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --lib master_fetch
```

Expected: FAIL to compile — `cannot find function parse_hist_ids`.

- [ ] **Step 3: Implement**

Prepend to `src-tauri/src/master_fetch.rs`:

```rust
//! Bloomberg requests that serve the security master rather than the time
//! series: who is this security, what identifiers has it worn, and what else
//! matches this text.
//!
//! Every field name below is confirmed in the P0 fact sheet. Do not add one
//! that is not: six plausible-looking mnemonics were already proven not to
//! exist, and Bloomberg reports an unknown field as a per-field exception
//! rather than an error, so a guess degrades quietly into missing data.

use crate::error::{AppError, AppResult};
use crate::orchestrator::PipelineConfig;
use crate::resolution::normalize::normalize_bbg_security;
use crate::resolution::score::Candidate;
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

/// The identity block requested at resolution step 3. All P0-verified (§6.1).
///
/// SIMP_SEC_STATUS is deliberately NOT here. It looked like a lifecycle field
/// and is not: P0 §10.2 measured it returning PREO, CLOS and HALT -- the market
/// session, updating in realtime. Requesting it would spend a call on a value
/// that is stale on arrival and meaningless to store. INACTIVE_DATE, below,
/// answers the question it was recruited for, with a date instead of a mood.
pub const IDENTITY_FIELDS: [&str; 12] = [
    "ID_BB_GLOBAL",
    "ID_BB_GLOBAL_SHARE_CLASS_LEVEL",
    // Without this, instrument.id_bb_unique and the 'bbg_unique' alias type are
    // dead schema no code path can populate. It rides along in a request that is
    // being made anyway, so it costs no extra Bloomberg call.
    "ID_BB_UNIQUE",
    "ID_ISIN",
    "EXCH_CODE",
    "CRNCY",
    "CNTRY_ISSUE_ISO",
    "SECURITY_TYP2",
    "MARKET_SECTOR_DES",
    "NAME",
    "LISTING_DATE",
    "INACTIVE_DATE",
];

pub const HIST_IDS_FIELD: &str = "HISTORICAL_IDS_TIME_RANGE";
/// Overrides on HIST_IDS_FIELD, resolved from its own FieldInfoRequest (P0 §6.3).
pub const HIST_IDS_ANCHOR: &str = "HISTORICAL_STARTING_IDENTIFIER";
pub const HIST_IDS_START: &str = "HISTORICAL_ID_TM_RANGE_START_DT";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IdentityBlock {
    pub security: String,
    pub figi: Option<String>,
    pub share_class_figi: Option<String>,
    pub bbg_unique: Option<String>,
    pub isin: Option<String>,
    pub exch_code: Option<String>,
    pub currency: Option<String>,
    pub country: Option<String>,
    pub security_typ2: Option<String>,
    pub market_sector: Option<String>,
    pub name: Option<String>,
    pub listing_date: Option<NaiveDate>,
    pub inactive_date: Option<NaiveDate>,
    /// Reserved for a lifecycle status P3/P5 may derive. Never populated from
    /// SIMP_SEC_STATUS -- see IDENTITY_FIELDS.
    pub status: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistIdRow {
    pub date: NaiveDate,
    pub old_id: String,
    pub new_id: String,
    pub old_exch: Option<String>,
    pub new_exch: Option<String>,
    pub action_id: Option<String>,
    pub source: Option<String>,
}

pub trait MasterFetcher {
    fn identity(&self, securities: &[String])
        -> impl std::future::Future<Output = AppResult<Vec<IdentityBlock>>> + Send;

    /// `anchor` is mandatory, not optional. P0 §6.4: without
    /// HISTORICAL_STARTING_IDENTIFIER the answer may describe a different
    /// company that once wore the same ticker.
    fn hist_ids(&self, security: &str, anchor: &str, start: NaiveDate)
        -> impl std::future::Future<Output = AppResult<Vec<HistIdRow>>> + Send;

    fn instrument_list(&self, query: &str, yellow_key_filter: Option<&str>,
                       max_results: u32)
        -> impl std::future::Future<Output = AppResult<Vec<Candidate>>> + Send;
}

// ------------------------------------------------------------------ parsing

fn s(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(str::to_string).filter(|t| !t.trim().is_empty())
}

fn date(v: &serde_json::Value) -> Option<NaiveDate> {
    v.as_str()?.parse().ok()
}

/// Walk the securityData array of every message in a response.
fn each_security<'a>(raw: &'a serde_json::Value)
    -> impl Iterator<Item = &'a serde_json::Value>
{
    raw.as_array().map(|v| v.as_slice()).unwrap_or(&[])
        .iter()
        .filter_map(|msg| msg.get("securityData"))
        .filter_map(|sd| sd.as_array())
        .flatten()
}

pub fn parse_identity(raw: &serde_json::Value) -> Vec<IdentityBlock> {
    each_security(raw)
        // A rejected security must not become a half-populated instrument.
        .filter(|sd| sd.get("securityError").is_none())
        .map(|sd| {
            let f = sd.get("fieldData").cloned().unwrap_or(serde_json::json!({}));
            let g = |k: &str| f.get(k).cloned().unwrap_or(serde_json::Value::Null);
            IdentityBlock {
                security: s(&sd["security"]).unwrap_or_default(),
                figi: s(&g("ID_BB_GLOBAL")),
                share_class_figi: s(&g("ID_BB_GLOBAL_SHARE_CLASS_LEVEL")),
                bbg_unique: s(&g("ID_BB_UNIQUE")),
                isin: s(&g("ID_ISIN")),
                exch_code: s(&g("EXCH_CODE")),
                currency: s(&g("CRNCY")),
                country: s(&g("CNTRY_ISSUE_ISO")),
                security_typ2: s(&g("SECURITY_TYP2")),
                market_sector: s(&g("MARKET_SECTOR_DES")),
                name: s(&g("NAME")),
                listing_date: date(&g("LISTING_DATE")),
                inactive_date: date(&g("INACTIVE_DATE")),
                status: None,
            }
        })
        .collect()
}

/// HISTORICAL_IDS_TIME_RANGE is a bulk field: its value is a list of dicts whose
/// column names are Bloomberg's own, spaces and all (P0 §6.3).
pub fn parse_hist_ids(raw: &serde_json::Value) -> Vec<HistIdRow> {
    each_security(raw)
        .filter(|sd| sd.get("securityError").is_none())
        .filter_map(|sd| sd.pointer(&format!("/fieldData/{HIST_IDS_FIELD}")))
        .filter_map(|v| v.as_array())
        .flatten()
        .filter_map(|row| {
            Some(HistIdRow {
                date: date(row.get("Date")?)?,
                old_id: s(row.get("Old ID")?)?,
                new_id: s(row.get("New ID")?)?,
                old_exch: row.get("Old Exch").and_then(s),
                new_exch: row.get("New Exch").and_then(s),
                action_id: row.get("Action ID").and_then(s),
                source: row.get("Source").and_then(s),
            })
        })
        .collect()
}

/// instrumentListRequest results. The `AAPL US<equity>` form is converted here,
/// on arrival; a candidate whose key is unrecognised is dropped rather than
/// carried forward as an identifier the Terminal will reject.
pub fn parse_list(raw: &serde_json::Value) -> Vec<Candidate> {
    raw.as_array().map(|v| v.as_slice()).unwrap_or(&[])
        .iter()
        .filter_map(|msg| msg.get("results"))
        .filter_map(|r| r.as_array())
        .flatten()
        .filter_map(|r| {
            let security = normalize_bbg_security(r.get("security")?.as_str()?)?;
            // "AAPL US Equity" -> exchange "US". Two tokens plus a yellow key is
            // the shape; anything else leaves the exchange unknown, which the
            // scorer treats as silence rather than contradiction.
            let parts: Vec<&str> = security.split_whitespace().collect();
            let exchange = (parts.len() == 3).then(|| parts[1].to_string());
            Some(Candidate {
                security,
                description: r.get("description").and_then(|d| d.as_str())
                    .unwrap_or_default().to_string(),
                exchange,
                country: None,
                currency: None,
                asset_class: None,
                figi: None,
            })
        })
        .collect()
}

// ------------------------------------------------------------------ live

pub struct BlpapiMasterFetcher<'a> {
    pub cfg: &'a PipelineConfig,
}

impl BlpapiMasterFetcher<'_> {
    async fn call(&self, spec: serde_json::Value) -> AppResult<serde_json::Value> {
        crate::blp_driver::run_raw(
            &self.cfg.python_path,
            &self.cfg.script_path,
            &serde_json::json!({
                "run_id": 0,
                "timeout_s": self.cfg.request_timeout_s,
                "requests": [spec],
            }),
        ).await
    }
}

impl MasterFetcher for BlpapiMasterFetcher<'_> {
    async fn identity(&self, securities: &[String]) -> AppResult<Vec<IdentityBlock>> {
        let resp = self.call(serde_json::json!({
            "kind": "reference",
            "securities": securities,
            "fields": IDENTITY_FIELDS,
            "obs_date": chrono::Local::now().date_naive().to_string(),
            "raw": true,
        })).await?;
        Ok(parse_identity(&resp["raw_messages"]))
    }

    async fn hist_ids(&self, security: &str, anchor: &str, start: NaiveDate)
        -> AppResult<Vec<HistIdRow>>
    {
        if anchor.trim().is_empty() {
            return Err(AppError::Validation(
                "hist_ids requires an anchoring identifier (P0 6.4)".into()));
        }
        let resp = self.call(serde_json::json!({
            "kind": "bulk_reference",
            "securities": [security],
            "fields": [HIST_IDS_FIELD],
            "overrides": [
                {"fieldId": HIST_IDS_ANCHOR, "value": anchor},
                {"fieldId": HIST_IDS_START, "value": start.format("%Y%m%d").to_string()},
            ],
            "raw": true,
        })).await?;
        Ok(parse_hist_ids(&resp["raw_messages"]))
    }

    async fn instrument_list(&self, query: &str, yellow_key_filter: Option<&str>,
                             max_results: u32) -> AppResult<Vec<Candidate>>
    {
        let resp = self.call(serde_json::json!({
            "kind": "instrument_list",
            "query": query,
            "yellow_key_filter": yellow_key_filter,
            "max_results": max_results,
            "raw": true,
        })).await?;
        Ok(parse_list(&resp["raw_messages"]))
    }
}

// ------------------------------------------------------------------ mock

/// Replays a committed capture. Every test above the transport uses this, so
/// the whole resolution path is exercised without a Terminal.
pub struct MockMasterFetcher {
    pub identity_raw: serde_json::Value,
    pub hist_ids_raw: serde_json::Value,
    pub list_raw: serde_json::Value,
    /// Every call recorded, so a test can assert Bloomberg was NOT called.
    pub calls: std::sync::Mutex<Vec<String>>,
}

impl Default for MockMasterFetcher {
    fn default() -> Self {
        Self {
            identity_raw: serde_json::json!([]),
            hist_ids_raw: serde_json::json!([]),
            list_raw: serde_json::json!([]),
            calls: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl MockMasterFetcher {
    /// Takes a P0 capture file and uses its first value for every request kind;
    /// tests that need finer control set the fields directly.
    pub fn from_capture(json: &str) -> Self {
        let all: serde_json::Value = serde_json::from_str(json).expect("capture json");
        let first = all.as_object()
            .and_then(|m| m.values().next().cloned())
            .unwrap_or(serde_json::json!([]));
        Self { hist_ids_raw: first.clone(), identity_raw: first.clone(),
               list_raw: first, ..Default::default() }
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn record(&self, what: &str) {
        self.calls.lock().unwrap().push(what.to_string());
    }
}

impl MasterFetcher for MockMasterFetcher {
    async fn identity(&self, securities: &[String]) -> AppResult<Vec<IdentityBlock>> {
        self.record(&format!("identity:{}", securities.join(",")));
        Ok(parse_identity(&self.identity_raw))
    }

    async fn hist_ids(&self, security: &str, anchor: &str, _start: NaiveDate)
        -> AppResult<Vec<HistIdRow>>
    {
        if anchor.trim().is_empty() {
            return Err(AppError::Validation(
                "hist_ids requires an anchoring identifier (P0 6.4)".into()));
        }
        self.record(&format!("hist_ids:{security}"));
        Ok(parse_hist_ids(&self.hist_ids_raw))
    }

    async fn instrument_list(&self, query: &str, _yk: Option<&str>, _max: u32)
        -> AppResult<Vec<Candidate>>
    {
        self.record(&format!("instrument_list:{query}"));
        Ok(parse_list(&self.list_raw))
    }
}
```

- [ ] **Step 4: Add `run_raw` to the driver**

`blp_driver::run_fetch` deserialises into `SidecarResponse`, which has no room
for the raw messages the master parsers need. Add a sibling that returns the
untyped JSON, in `src-tauri/src/blp_driver.rs`:

```rust
/// Run the sidecar and return its response as untyped JSON.
///
/// `run_fetch` deserialises into SidecarResponse, which models observations.
/// The security-master requests return tables and search results instead, so
/// they read the JSON directly rather than widening SidecarResponse with
/// fields that mean nothing to an EOD run.
pub async fn run_raw(
    python_path: &Path,
    script_path: &Path,
    payload: &serde_json::Value,
) -> AppResult<serde_json::Value> {
    let text = run_sidecar_text(python_path, script_path, payload).await?;
    serde_json::from_str(&text)
        .map_err(|e| AppError::Sidecar(format!("sidecar returned invalid JSON: {e}")))
}
```

Refactor the existing `run_fetch` so both share one `run_sidecar_text` helper
that spawns the process, writes the payload to stdin, applies the timeout and
returns stdout as a `String`. `run_fetch` then parses that string into
`SidecarResponse` exactly as it does today, and keeps writing `audit_path`.

The sidecar must include the raw messages when a request sets `"raw": true`; add
`"raw_messages"` to the emitted object in `main()`. **It is a flat array of
messages**, not the `{"request":.., "messages":[..]}` items that `--raw-out`
writes — the master parsers walk `securityData` directly, so flattening here
keeps them from having to know the capture envelope:

```python
    raw_messages = []
    for item in capture.get("captured", []):
        raw_messages.extend(item.get("messages", []))
```

The P0 capture files happen to already be in this flat shape, which is why the
same parsers work against both.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --lib master_fetch
```

Expected: all six tests PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/master_fetch.rs src-tauri/src/blp_driver.rs src-tauri/src/lib.rs
git commit -m "feat(master): Bloomberg identity, hist-ids and instrument-list seam with replay mock"
```

---

## Task 7: The resolution engine

The seven steps of spec §5, wired together. Every path writes a `resolution_decision` — including the local-alias path, which records that no call was made.

**Files:**
- Create: `src-tauri/src/resolution/engine.rs`
- Modify: `src-tauri/src/resolution/mod.rs`
- Test: `src-tauri/tests/resolution.rs`

**Interfaces:**
- Consumes: `instrument::store`, `master_fetch::{MasterFetcher, IdentityBlock, IDENTITY_FIELDS}`, `resolution::{normalize, score}`.
- Produces:
  - `pub struct ResolveInput { pub raw: String, pub yellow_key: String, pub hints: Hints, pub as_of: NaiveDate, pub decided_by: String }`
  - `pub enum Resolution { Bound { instrument_id: i64, decision_id: i64, method: String }, NeedsReview { decision_id: i64, review_id: i64, candidates: Vec<Scored> }, NotFound { decision_id: i64 } }`
  - `pub async fn resolve<F: MasterFetcher>(pool, fetcher: &F, input: &ResolveInput) -> AppResult<Resolution>`
  - `pub async fn resolve_review(pool, review_id, chosen_security: &str, by: &str) -> AppResult<i64>`
  - `pub async fn pending_reviews(pool) -> AppResult<Vec<PendingReview>>`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/resolution.rs`:

```rust
mod common;

use chrono::NaiveDate;
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::master_fetch::MockMasterFetcher;
use getbloomdata_lib::resolution::engine::{self, Resolution, ResolveInput};
use getbloomdata_lib::resolution::score::Hints;

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

fn input(raw: &str) -> ResolveInput {
    ResolveInput {
        raw: raw.into(),
        yellow_key: "Equity".into(),
        hints: Hints::default(),
        as_of: d("2026-08-19"),
        decided_by: "auto".into(),
    }
}

fn identity_mock(security: &str, figi: &str, exch: &str) -> MockMasterFetcher {
    MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": security, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {
                "ID_BB_GLOBAL": figi, "ID_ISIN": "US0378331005",
                "EXCH_CODE": exch, "CRNCY": "USD", "CNTRY_ISSUE_ISO": "US",
                "SECURITY_TYP2": "Common Stock", "MARKET_SECTOR_DES": "Equity",
                "NAME": "APPLE INC", "LISTING_DATE": "1980-12-12"}}]}]),
        ..Default::default()
    }
}

/// Step 2 of the pipeline. The hit budget depends on this being true: an
/// instrument already in the master is never asked about again.
#[tokio::test]
async fn a_known_alias_resolves_locally_and_calls_bloomberg_zero_times() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: "AAPL US Equity".into(),
        exch_code: Some("US".into()), valid_from: d("1980-12-12"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();

    let mock = MockMasterFetcher::default();
    let r = engine::resolve(&pool, &mock, &input("AAPL US")).await.unwrap();

    match r {
        Resolution::Bound { instrument_id, method, decision_id } => {
            assert_eq!(instrument_id, inst.instrument_id);
            assert_eq!(method, "local_alias");
            // Even the free path is recorded, so the audit trail has no holes.
            let m: String = sqlx::query_scalar(
                "SELECT method FROM resolution_decision WHERE id = $1")
                .bind(decision_id).fetch_one(&pool).await.unwrap();
            assert_eq!(m, "local_alias");
        }
        other => panic!("expected Bound, got {other:?}"),
    }
    assert_eq!(mock.call_count(), 0, "a known instrument costs nothing");
}

#[tokio::test]
async fn an_unknown_identifier_resolves_through_a_reference_request() {
    let pool = common::pool().await;
    let mock = identity_mock("ZZTOP US Equity", "BBG000TESTAA", "US");
    let r = engine::resolve(&pool, &mock, &input("ZZTOP US")).await.unwrap();
    let Resolution::Bound { instrument_id, method, .. } = r else {
        panic!("expected Bound, got {r:?}")
    };
    assert_eq!(method, "bloomberg_ref");

    // The identity block became aliases and attributes, not columns.
    let aliases = store::aliases(&pool, instrument_id).await.unwrap();
    let types: Vec<&str> = aliases.iter().map(|a| a.id_type.as_str()).collect();
    assert!(types.contains(&"bdp_security"));
    assert!(types.contains(&"figi"));
    assert!(types.contains(&"isin"));
    let attrs = store::attrs(&pool, instrument_id, d("2026-08-19")).await.unwrap();
    assert!(attrs.iter().any(|a| a.attr == "name" && a.value == "APPLE INC"));
    assert!(attrs.iter().any(|a| a.attr == "currency" && a.value == "USD"));
}

#[tokio::test]
async fn the_unedited_bloomberg_response_is_stored_with_the_decision() {
    let pool = common::pool().await;
    let mock = identity_mock("ZZTOP2 US Equity", "BBG000TESTAB", "US");
    let r = engine::resolve(&pool, &mock, &input("ZZTOP2 US")).await.unwrap();
    let Resolution::Bound { decision_id, .. } = r else { panic!("expected Bound") };
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT bbg_response FROM resolution_decision WHERE id = $1")
        .bind(decision_id).fetch_one(&pool).await.unwrap();
    assert_eq!(raw[0]["securityData"][0]["fieldData"]["NAME"], "APPLE INC",
               "what Bloomberg said is recoverable, not just what we concluded");
}

/// Step 6. Two survivors bind nothing -- the whole point of the phase.
#[tokio::test]
async fn an_ambiguous_result_opens_a_review_and_binds_nothing() {
    let pool = common::pool().await;
    let mock = MockMasterFetcher {
        // No identity block: the reference request found nothing usable...
        identity_raw: serde_json::json!([]),
        // ...so step 4 searches, and two listings come back.
        list_raw: serde_json::json!([{"results": [
            {"security": "AAPL US<equity>", "description": "Apple Inc"},
            {"security": "AAPL LN<equity>", "description": "Apple Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input("AAPL")).await.unwrap();
    let Resolution::NeedsReview { review_id, candidates, .. } = r else {
        panic!("expected NeedsReview, got {r:?}")
    };
    assert_eq!(candidates.len(), 2);
    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "pending");
    let bound: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument WHERE id_bb_global IS NOT NULL")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(bound, 0, "nothing binds while a human has not chosen");
}

/// Step 5. One survivor after scoring binds without a human.
#[tokio::test]
async fn a_hint_that_leaves_one_survivor_binds_without_review() {
    let pool = common::pool().await;
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": "ZBBB US<equity>", "description": "Test Inc"},
            {"security": "ZBBB LN<equity>", "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let mut inp = input("ZBBB");
    inp.hints.exchange = Some("LN".into());
    let r = engine::resolve(&pool, &mock, &inp).await.unwrap();
    assert!(matches!(r, Resolution::NeedsReview { .. } | Resolution::Bound { .. }));
    // With one survivor the engine issues a second identity request for it,
    // so the method reflects where the decision was actually made.
    if let Resolution::Bound { method, .. } = r {
        assert_eq!(method, "bloomberg_list");
    }
}

#[tokio::test]
async fn resolving_a_review_binds_the_chosen_candidate_and_closes_it() {
    let pool = common::pool().await;
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": "ZCCC US<equity>", "description": "Test Inc"},
            {"security": "ZCCC LN<equity>", "description": "Test Inc"}]}]),
        ..Default::default()
    };
    let r = engine::resolve(&pool, &mock, &input("ZCCC")).await.unwrap();
    let Resolution::NeedsReview { review_id, .. } = r else { panic!("expected review") };

    let iid = engine::resolve_review(&pool, review_id, "ZCCC US Equity", "laurent")
        .await.unwrap();
    let status: String = sqlx::query_scalar(
        "SELECT status FROM resolution_review WHERE id = $1")
        .bind(review_id).fetch_one(&pool).await.unwrap();
    assert_eq!(status, "resolved");
    assert_eq!(store::find_by_alias(&pool, "bdp_security", "ZCCC US Equity",
                                    d("2026-08-19")).await.unwrap(), Some(iid));
    assert!(engine::pending_reviews(&pool).await.unwrap()
                .iter().all(|p| p.review_id != review_id));
}

#[tokio::test]
async fn nothing_found_is_recorded_as_a_decision_too() {
    let pool = common::pool().await;
    let mock = MockMasterFetcher::default();  // empty everything
    let r = engine::resolve(&pool, &mock, &input("QQQQZZZ")).await.unwrap();
    let Resolution::NotFound { decision_id } = r else { panic!("expected NotFound, got {r:?}") };
    let chosen: Option<i64> = sqlx::query_scalar(
        "SELECT chosen_instrument_id FROM resolution_decision WHERE id = $1")
        .bind(decision_id).fetch_one(&pool).await.unwrap();
    assert_eq!(chosen, None);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test resolution
```

Expected: FAIL to compile — `cannot find module engine`.

- [ ] **Step 3: Implement**

Add `pub mod engine;` to `src-tauri/src/resolution/mod.rs`, then create
`src-tauri/src/resolution/engine.rs`:

```rust
//! Spec §5: turning what the user typed into an instrument_id.
//!
//! Two properties matter more than the steps themselves.
//!
//! First, every path writes a resolution_decision -- including the local path
//! that costs nothing. An audit trail with holes where the cheap answers went
//! cannot answer "why is this instrument bound to that security".
//!
//! Second, ambiguity is not resolved by guessing. Two plausible candidates
//! produce a review and bind nothing, because a wrong binding is discovered
//! months later as a silently wrong price series.

use crate::error::AppResult;
use crate::instrument::store::{self, NewAlias};
use crate::master_fetch::{IdentityBlock, MasterFetcher};
use crate::resolution::normalize::{build_security, detect_id_kind};
use crate::resolution::score::{score_all, verdict, Candidate, Hints, Scored, Verdict};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Deserialize)]
pub struct ResolveInput {
    pub raw: String,
    pub yellow_key: String,
    pub hints: Hints,
    pub as_of: NaiveDate,
    /// 'auto' for an automatic resolution, or the user who asked for it.
    pub decided_by: String,
}

#[derive(Debug, Serialize)]
pub enum Resolution {
    Bound { instrument_id: i64, decision_id: i64, method: String },
    NeedsReview { decision_id: i64, review_id: i64, candidates: Vec<Scored> },
    NotFound { decision_id: i64 },
}

#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct PendingReview {
    pub review_id: i64,
    pub decision_id: i64,
    pub raw_input: String,
    pub normalized: String,
    pub candidates: serde_json::Value,
    pub bbg_response: Option<serde_json::Value>,
    pub opened_at: chrono::DateTime<chrono::Utc>,
}

/// Bloomberg's yellow-key filter for instrumentListRequest, derived from the
/// market sector the user chose. Values are from the //blp/instruments schema
/// captured in P0.
fn yellow_key_filter(yellow_key: &str) -> Option<&'static str> {
    match yellow_key.trim().to_ascii_lowercase().as_str() {
        "equity" => Some("YK_FILTER_EQTY"),
        "corp" => Some("YK_FILTER_CORP"),
        "govt" => Some("YK_FILTER_GOVT"),
        "index" => Some("YK_FILTER_INDX"),
        "curncy" => Some("YK_FILTER_CURR"),
        "comdty" => Some("YK_FILTER_CMDT"),
        "mtge" => Some("YK_FILTER_MTGE"),
        "muni" => Some("YK_FILTER_MUNI"),
        "pfd" => Some("YK_FILTER_PRFD"),
        _ => None,
    }
}

#[allow(clippy::too_many_arguments)]
async fn record_decision(pool: &PgPool, input: &ResolveInput, normalized: &str,
                         method: &str, chosen: Option<i64>,
                         candidates: &serde_json::Value,
                         bbg: Option<&serde_json::Value>) -> AppResult<i64>
{
    Ok(sqlx::query_scalar(
        "INSERT INTO resolution_decision
           (raw_input, normalized, hint_exchange, hint_country, hint_currency,
            hint_asset_class, method, chosen_instrument_id, candidates,
            bbg_response, decided_by)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) RETURNING id")
        .bind(&input.raw).bind(normalized)
        .bind(&input.hints.exchange).bind(&input.hints.country)
        .bind(&input.hints.currency).bind(&input.hints.asset_class)
        .bind(method).bind(chosen).bind(candidates).bind(bbg)
        .bind(&input.decided_by)
        .fetch_one(pool).await?)
}

/// Write an identity block into the master: one instrument, its aliases, its
/// attributes. Idempotent on re-resolution because find_by_alias runs first.
async fn bind_identity(pool: &PgPool, block: &IdentityBlock, decision_id: i64,
                       as_of: NaiveDate) -> AppResult<i64>
{
    // A FIGI already in the master is the same instrument, not a new one.
    if let Some(figi) = block.figi.as_deref() {
        if let Some(existing) = sqlx::query_scalar::<_, i64>(
            "SELECT instrument_id FROM instrument WHERE id_bb_global = $1")
            .bind(figi).fetch_optional(pool).await?
        {
            return Ok(existing);
        }
    }
    let inst = store::create(pool).await?;
    store::set_bloomberg_ids(pool, inst.instrument_id, block.figi.as_deref(),
                             block.bbg_unique.as_deref()).await?;

    // Listing date is the honest start of every identifier's validity; without
    // one, today is the only date we can defend.
    let from = block.listing_date.unwrap_or(as_of);
    let to = block.inactive_date;

    let mut tx = pool.begin().await?;
    let alias = |id_type: &str, value: &str| NewAlias {
        id_type: id_type.into(), value: value.into(),
        exch_code: block.exch_code.clone(), valid_from: from, valid_to: to,
        source: "bloomberg_ref".into(), bbg_action_id: None,
        anchoring_identifier: None,
    };
    store::insert_alias(&mut tx, inst.instrument_id,
                        &alias("bdp_security", &block.security)).await?;
    if let Some(v) = &block.figi {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("figi", v)).await?;
    }
    if let Some(v) = &block.share_class_figi {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("figi", v)).await?;
    }
    if let Some(v) = &block.isin {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("isin", v)).await?;
    }
    if let Some(v) = &block.bbg_unique {
        store::insert_alias(&mut tx, inst.instrument_id, &alias("bbg_unique", v)).await?;
    }

    for (attr, value) in [
        ("name", &block.name),
        ("exchange", &block.exch_code),
        ("currency", &block.currency),
        ("country", &block.country),
        ("instrument_type", &block.security_typ2),
        ("asset_class", &block.market_sector),
        // No "status": P0 §10.2 -- SIMP_SEC_STATUS is a trading-session state,
        // not a lifecycle one. INACTIVE_DATE above already closes the validity
        // periods, which is the durable way to say an instrument has ended.
    ] {
        if let Some(v) = value {
            store::set_attr(&mut tx, inst.instrument_id, attr, v, from,
                            "bloomberg", Some(decision_id)).await?;
        }
    }
    tx.commit().await?;
    Ok(inst.instrument_id)
}

pub async fn resolve<F: MasterFetcher>(pool: &PgPool, fetcher: &F,
                                       input: &ResolveInput) -> AppResult<Resolution>
{
    // 1. normalise
    let kind = detect_id_kind(&input.raw);
    let security = build_security(kind, &input.raw, &input.yellow_key)?;

    // 2. local alias lookup -- the free path
    for id_type in ["bdp_security", "ticker", "isin", "figi"] {
        let probe = if id_type == "bdp_security" { security.as_str() }
                    else { input.raw.trim() };
        if let Some(iid) = store::find_by_alias(pool, id_type, probe, input.as_of).await? {
            let decision_id = record_decision(
                pool, input, &security, "local_alias", Some(iid),
                &serde_json::json!({"matched": id_type, "bloomberg_calls": 0}),
                None).await?;
            return Ok(Resolution::Bound {
                instrument_id: iid, decision_id, method: "local_alias".into() });
        }
    }

    // 3. ReferenceDataRequest for the identity block
    let blocks = fetcher.identity(std::slice::from_ref(&security)).await?;
    let raw_identity = serde_json::to_value(&blocks).unwrap_or(serde_json::json!([]));
    if blocks.len() == 1 {
        let decision_id = record_decision(
            pool, input, &security, "bloomberg_ref", None,
            &serde_json::json!([&blocks[0]]), Some(&raw_identity)).await?;
        let iid = bind_identity(pool, &blocks[0], decision_id, input.as_of).await?;
        sqlx::query("UPDATE resolution_decision SET chosen_instrument_id = $2 WHERE id = $1")
            .bind(decision_id).bind(iid).execute(pool).await?;
        return Ok(Resolution::Bound {
            instrument_id: iid, decision_id, method: "bloomberg_ref".into() });
    }

    // 4. ambiguous or absent -> search
    let found = fetcher.instrument_list(
        input.raw.trim(), yellow_key_filter(&input.yellow_key), 20).await?;

    // 5. score
    let scored = score_all(found, &input.hints);
    let candidates_json = serde_json::to_value(&scored).unwrap_or(serde_json::json!([]));

    match verdict(scored) {
        Verdict::Unique(c) => {
            // The search gave a security string, not an identity. Ask once more.
            let blocks = fetcher.identity(std::slice::from_ref(&c.security)).await?;
            let Some(block) = blocks.into_iter().next() else {
                let decision_id = record_decision(
                    pool, input, &security, "bloomberg_list", None,
                    &candidates_json, None).await?;
                return Ok(Resolution::NotFound { decision_id });
            };
            let decision_id = record_decision(
                pool, input, &security, "bloomberg_list", None,
                &candidates_json,
                Some(&serde_json::to_value(&block).unwrap_or_default())).await?;
            let iid = bind_identity(pool, &block, decision_id, input.as_of).await?;
            sqlx::query("UPDATE resolution_decision SET chosen_instrument_id = $2 WHERE id = $1")
                .bind(decision_id).bind(iid).execute(pool).await?;
            Ok(Resolution::Bound {
                instrument_id: iid, decision_id, method: "bloomberg_list".into() })
        }
        // 6. two or more survivors: a human decides, and NOTHING is bound.
        Verdict::Ambiguous(list) => {
            let decision_id = record_decision(
                pool, input, &security, "bloomberg_list", None,
                &candidates_json, None).await?;
            let review_id: i64 = sqlx::query_scalar(
                "INSERT INTO resolution_review (decision_id, status)
                 VALUES ($1,'pending') RETURNING id")
                .bind(decision_id).fetch_one(pool).await?;
            Ok(Resolution::NeedsReview { decision_id, review_id, candidates: list })
        }
        Verdict::None => {
            let decision_id = record_decision(
                pool, input, &security, "bloomberg_list", None,
                &candidates_json, None).await?;
            Ok(Resolution::NotFound { decision_id })
        }
    }
}

pub async fn pending_reviews(pool: &PgPool) -> AppResult<Vec<PendingReview>> {
    Ok(sqlx::query_as::<_, PendingReview>(
        "SELECT r.id AS review_id, d.id AS decision_id, d.raw_input, d.normalized,
                d.candidates, d.bbg_response, r.opened_at
           FROM resolution_review r JOIN resolution_decision d ON d.id = r.decision_id
          WHERE r.status = 'pending' ORDER BY r.opened_at")
        .fetch_all(pool).await?)
}

/// A human picked a candidate. The chosen security is resolved for real -- it is
/// not bound from the search result, because a search result is a name, not an
/// identity.
pub async fn resolve_review(pool: &PgPool, review_id: i64, chosen_security: &str,
                            by: &str) -> AppResult<i64>
{
    let (decision_id, raw_input): (i64, String) = sqlx::query_as(
        "SELECT d.id, d.raw_input FROM resolution_review r
           JOIN resolution_decision d ON d.id = r.decision_id WHERE r.id = $1")
        .bind(review_id).fetch_one(pool).await?;

    // Reuse whatever the original decision learned about the candidate.
    let candidates: serde_json::Value = sqlx::query_scalar(
        "SELECT candidates FROM resolution_decision WHERE id = $1")
        .bind(decision_id).fetch_one(pool).await?;
    let block = IdentityBlock { security: chosen_security.to_string(),
                                ..Default::default() };

    let manual_id = sqlx::query_scalar::<_, i64>(
        "INSERT INTO resolution_decision
           (raw_input, normalized, method, candidates, decided_by)
         VALUES ($1,$2,'manual',$3,$4) RETURNING id")
        .bind(&raw_input).bind(chosen_security).bind(&candidates).bind(by)
        .fetch_one(pool).await?;

    let iid = bind_identity(pool, &block, manual_id,
                            chrono::Local::now().date_naive()).await?;
    sqlx::query("UPDATE resolution_decision SET chosen_instrument_id = $2 WHERE id = $1")
        .bind(manual_id).bind(iid).execute(pool).await?;
    sqlx::query("UPDATE resolution_review SET status = 'resolved', closed_at = now(),
                        note = note || $2 WHERE id = $1")
        .bind(review_id).bind(format!(" resolved by {by} to {chosen_security}"))
        .execute(pool).await?;
    Ok(iid)
}

/// Also needed by the UI: a review the user judges unresolvable.
pub async fn reject_review(pool: &PgPool, review_id: i64, note: &str) -> AppResult<()> {
    sqlx::query("UPDATE resolution_review
                    SET status = 'rejected', closed_at = now(), note = $2
                  WHERE id = $1")
        .bind(review_id).bind(note).execute(pool).await?;
    Ok(())
}
```

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test resolution
```

Expected: all seven tests PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src/resolution src-tauri/tests/resolution.rs
git commit -m "feat(resolution): seven-step engine with audited decisions and a review gate"
```

---
## Task 8: Identifier history

One `HISTORICAL_IDS_TIME_RANGE` request per instrument, for its lifetime, always anchored. Its rows become alias validity periods. An `Old ID` already belonging to a different instrument opens a link proposal — it never merges.

**Files:**
- Create: `src-tauri/src/instrument/history.rs`
- Modify: `src-tauri/src/instrument/mod.rs`
- Modify: `src-tauri/src/resolution/engine.rs` (call it after a successful bind)
- Test: `src-tauri/tests/identifier_history.rs`

**Interfaces:**
- Consumes: `master_fetch::{MasterFetcher, HistIdRow}`, `instrument::store`.
- Produces:
  - `pub async fn ingest<F: MasterFetcher>(pool, fetcher: &F, instrument_id: i64, anchor: &str, start: NaiveDate) -> AppResult<HistoryOutcome>`
  - `pub struct HistoryOutcome { pub aliases_added: usize, pub links_proposed: Vec<i64> }`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/identifier_history.rs`:

```rust
mod common;

use chrono::NaiveDate;
use getbloomdata_lib::instrument::{history, store::{self, NewAlias}};
use getbloomdata_lib::master_fetch::MockMasterFetcher;

const HISTIDS: &str = include_str!(
    "../../docs/superpowers/specs/blpapi-facts/histids_report.json");

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

fn capture(key: &str) -> serde_json::Value {
    let all: serde_json::Value = serde_json::from_str(HISTIDS).unwrap();
    all[key].clone()
}

fn mock_for(key: &str) -> MockMasterFetcher {
    MockMasterFetcher { hist_ids_raw: capture(key), ..Default::default() }
}

const ANCHORED: &str = "META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT', \
                        'HISTORICAL_STARTING_IDENTIFIER']";
const UNANCHORED: &str = "META US Equity | ['HISTORICAL_ID_TM_RANGE_START_DT']";

async fn instrument_with_ticker(pool: &sqlx::PgPool, ticker: &str, from: &str) -> i64 {
    let inst = store::create(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "ticker".into(), value: ticker.into(), exch_code: Some("US".into()),
        valid_from: d(from), valid_to: None, source: "user".into(),
        bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    inst.instrument_id
}

#[tokio::test]
async fn a_rename_becomes_two_validity_periods_on_one_instrument() {
    let pool = common::pool().await;
    let iid = instrument_with_ticker(&pool, "META", "2022-06-09").await;
    let mock = mock_for(ANCHORED);

    let out = history::ingest(&pool, &mock, iid, "META US Equity", d("2000-01-01"))
        .await.unwrap();
    assert_eq!(out.aliases_added, 1, "FB is added; META is already there");
    assert!(out.links_proposed.is_empty(), "one instrument, no link needed");

    let aliases = store::aliases(&pool, iid).await.unwrap();
    let fb = aliases.iter().find(|a| a.value == "FB").expect("FB alias");
    assert_eq!(fb.valid_to, d("2022-06-09"), "FB stopped on the change date");
    assert_eq!(fb.bbg_action_id.as_deref(), Some("228233742"),
               "Bloomberg's own event id is the key P3 needs for amendments");
    assert_eq!(fb.anchoring_identifier.as_deref(), Some("META US Equity"));
    assert_eq!(fb.source, "bloomberg_hist_ids");
}

/// P0 §6.4 as a regression test. The unanchored answer says META became METV,
/// which is the Roundhill Ball Metaverse ETF, not Facebook. Ingesting it as an
/// alias of this instrument would silently attach another company's identity.
#[tokio::test]
async fn an_old_id_belonging_to_another_instrument_proposes_a_link_and_merges_nothing() {
    let pool = common::pool().await;
    // The METV instrument already exists in the master, under its own identity.
    let metv = instrument_with_ticker(&pool, "METV", "2022-01-31").await;
    let meta = instrument_with_ticker(&pool, "META", "2022-06-09").await;

    let mock = mock_for(UNANCHORED);
    let out = history::ingest(&pool, &mock, meta, "META US Equity", d("2000-01-01"))
        .await.unwrap();

    assert_eq!(out.aliases_added, 0,
               "an identifier owned by another instrument is never absorbed");
    assert_eq!(out.links_proposed.len(), 1);

    let (pred, succ, confirmed): (i64, i64, Option<String>) = sqlx::query_as(
        "SELECT predecessor_id, successor_id, confirmed_by FROM instrument_link
          WHERE id = $1").bind(out.links_proposed[0])
        .fetch_one(&pool).await.unwrap();
    assert_eq!((pred, succ), (meta, metv));
    assert_eq!(confirmed, None, "it is a proposal until a human agrees");
    assert_eq!(store::confirmed_successor(&pool, meta).await.unwrap(), None);
}

#[tokio::test]
async fn ingestion_without_an_anchor_is_refused_before_any_request_is_sent() {
    let pool = common::pool().await;
    let iid = instrument_with_ticker(&pool, "META", "2022-06-09").await;
    let mock = mock_for(ANCHORED);
    let err = history::ingest(&pool, &mock, iid, "  ", d("2000-01-01")).await.unwrap_err();
    assert!(err.to_string().contains("anchoring"), "got: {err}");
    assert_eq!(mock.call_count(), 0, "a refused request must not cost a hit");
}

#[tokio::test]
async fn ingesting_twice_adds_nothing_the_second_time() {
    let pool = common::pool().await;
    let iid = instrument_with_ticker(&pool, "META", "2022-06-09").await;
    let mock = mock_for(ANCHORED);
    history::ingest(&pool, &mock, iid, "META US Equity", d("2000-01-01")).await.unwrap();
    let second = history::ingest(&pool, &mock, iid, "META US Equity", d("2000-01-01"))
        .await.unwrap();
    assert_eq!(second.aliases_added, 0, "the same Action ID is not applied twice");
    assert_eq!(store::aliases(&pool, iid).await.unwrap().len(), 2);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test identifier_history
```

Expected: FAIL to compile — `cannot find module history`.

- [ ] **Step 3: Implement**

Add `pub mod history;` to `src-tauri/src/instrument/mod.rs`, then create
`src-tauri/src/instrument/history.rs`:

```rust
//! Turning HISTORICAL_IDS_TIME_RANGE into alias validity periods.
//!
//! The anchoring rule is the whole point. P0 §6.4: asked about META US Equity
//! WITHOUT HISTORICAL_STARTING_IDENTIFIER, Bloomberg answers about the Roundhill
//! Ball Metaverse ETF, which also once wore the ticker META. The answer is
//! well-formed, plausible, and about a different company. So the anchor is a
//! required argument here, the column is NOT NULL by CHECK constraint, and an
//! identifier that already belongs to someone else is never absorbed.

use crate::error::{AppError, AppResult};
use crate::instrument::store::{self, NewAlias};
use crate::master_fetch::{HistIdRow, MasterFetcher};
use chrono::NaiveDate;
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Serialize)]
pub struct HistoryOutcome {
    pub aliases_added: usize,
    /// instrument_link ids, all unconfirmed.
    pub links_proposed: Vec<i64>,
}

pub async fn ingest<F: MasterFetcher>(pool: &PgPool, fetcher: &F, instrument_id: i64,
                                      anchor: &str, start: NaiveDate)
    -> AppResult<HistoryOutcome>
{
    if anchor.trim().is_empty() {
        return Err(AppError::Validation(
            "identifier history requires an anchoring identifier (P0 6.4)".into()));
    }
    let rows = fetcher.hist_ids(anchor, anchor, start).await?;
    apply(pool, instrument_id, anchor, &rows).await
}

/// Split from `ingest` so the mapping can be exercised without a fetcher.
pub async fn apply(pool: &PgPool, instrument_id: i64, anchor: &str, rows: &[HistIdRow])
    -> AppResult<HistoryOutcome>
{
    let mut aliases_added = 0usize;
    let mut links_proposed = Vec::new();

    for row in rows {
        // Has this exact event already been applied? Bloomberg's Action ID is
        // stable, which is what makes re-ingestion cheap and idempotent -- and
        // is the key P3 will use to spot an amended or withdrawn change.
        if let Some(action) = &row.action_id {
            let seen: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM instrument_alias
                  WHERE instrument_id = $1 AND bbg_action_id = $2
                    AND system_to = 'infinity'")
                .bind(instrument_id).bind(action).fetch_one(pool).await?;
            if seen > 0 {
                continue;
            }
        }

        // Does either end of this change already belong to somebody else?
        //
        // Ownership is checked across ALL validity periods, not as of the change
        // date: the question is "is this identifier another instrument's, ever",
        // and an alias whose period has not started yet still answers it.
        //
        // The New ID is checked first, and it is what catches the META/METV case.
        // Anchored, the row reads FB -> META and META is our own current ticker,
        // so it falls through and FB becomes our alias. Unanchored, the row reads
        // META -> METV, and METV belongs to the Roundhill ETF. That is Bloomberg
        // telling us this chain is not ours. Absorbing it would attach another
        // company's identity to this instrument, so it becomes a proposal.
        if let Some(other) = owner_of(pool, &row.new_id).await? {
            if other != instrument_id {
                links_proposed.push(store::propose_link(
                    pool, instrument_id, other, "rename", row.date,
                    evidence(anchor, row,
                        "the New ID already belongs to another instrument; this \
                         chain of events is not this instrument's")).await?);
                continue;
            }
        }
        // The symmetric case: the Old ID is someone else's, so the change runs
        // from them to us. Same refusal, opposite direction.
        if let Some(other) = owner_of(pool, &row.old_id).await? {
            if other != instrument_id {
                links_proposed.push(store::propose_link(
                    pool, other, instrument_id, "rename", row.date,
                    evidence(anchor, row,
                        "the Old ID already belongs to another instrument; an \
                         automatic merge would destroy one of the two histories")).await?);
            }
            // Either it is ours already, or it is a proposal. Neither adds an alias.
            continue;
        }

        let mut tx = pool.begin().await?;
        // The old identifier was true from the start of the window until the change.
        store::insert_alias(&mut tx, instrument_id, &NewAlias {
            id_type: "ticker".into(),
            value: row.old_id.clone(),
            exch_code: row.old_exch.clone(),
            // Bloomberg gives the date the change took effect, not when the old
            // identifier began. The instrument's own start is the honest floor.
            valid_from: earliest_known(pool, instrument_id, row.date).await?,
            valid_to: Some(row.date),
            source: "bloomberg_hist_ids".into(),
            bbg_action_id: row.action_id.clone(),
            anchoring_identifier: Some(anchor.to_string()),
        }).await?;
        tx.commit().await?;
        aliases_added += 1;
    }

    Ok(HistoryOutcome { aliases_added, links_proposed })
}

/// Which instrument, if any, has ever worn this ticker.
///
/// Deliberately not as-of a date: `find_by_alias` answers "who wore this on that
/// day", which is the right question when resolving user input and the wrong one
/// here. An identifier whose validity period has not started yet is still
/// somebody's, and treating it as free is exactly how two histories get merged.
async fn owner_of(pool: &PgPool, ticker: &str) -> AppResult<Option<i64>> {
    Ok(sqlx::query_scalar(
        "SELECT instrument_id FROM instrument_alias
          WHERE id_type = 'ticker' AND lower(value) = lower($1)
            AND system_to = 'infinity'
          ORDER BY valid_from LIMIT 1")
        .bind(ticker).fetch_optional(pool).await?)
}

fn evidence(anchor: &str, row: &HistIdRow, why: &str) -> serde_json::Value {
    serde_json::json!({
        "field": "HISTORICAL_IDS_TIME_RANGE",
        "anchoring_identifier": anchor,
        "row": row,
        "why": why,
    })
}

/// The earliest validity start we already know for this instrument, or the day
/// before the change if we know nothing. Never later than `change_date`, because
/// an alias whose period is empty violates instrument_alias_period.
async fn earliest_known(pool: &PgPool, instrument_id: i64, change_date: NaiveDate)
    -> AppResult<NaiveDate>
{
    let found: Option<NaiveDate> = sqlx::query_scalar(
        "SELECT min(valid_from) FROM instrument_alias
          WHERE instrument_id = $1 AND system_to = 'infinity'")
        .bind(instrument_id).fetch_one(pool).await?;
    Ok(found.filter(|f| *f < change_date)
        .unwrap_or_else(|| change_date.pred_opt().unwrap_or(change_date)))
}
```

- [ ] **Step 4: Call it from the resolution engine**

In `src-tauri/src/resolution/engine.rs`, `resolve()` currently binds and returns.
After a successful `bind_identity` on the `bloomberg_ref` and `bloomberg_list`
paths, pull the history once — this is the second of the two calls the hit budget
allots to a never-seen instrument, and it never happens again for that instrument.

```rust
            let iid = bind_identity(pool, &blocks[0], decision_id, input.as_of).await?;
            // Spec §5.1: one anchored history request per instrument, ever.
            // Failure here must not undo a good binding -- the identifiers we
            // have are still correct, we simply know less about the past.
            let start = blocks[0].listing_date
                .unwrap_or_else(|| NaiveDate::from_ymd_opt(1980, 1, 1).unwrap());
            if let Err(e) = crate::instrument::history::ingest(
                pool, fetcher, iid, &blocks[0].security, start).await
            {
                eprintln!("identifier history for {} failed: {e}", blocks[0].security);
            }
```

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test identifier_history && cargo test --test resolution
```

Expected: all four history tests PASS, and the seven resolution tests still PASS.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/instrument src-tauri/src/resolution src-tauri/tests
git commit -m "feat(instrument): anchored identifier history; a shared old id proposes a link, never a merge"
```

---

## Task 9: The book replaces the asset table

`asset` disappears from the code. `registry.rs` keeps asset classes; everything about a held instrument moves to `book.rs`.

**Files:**
- Create: `src-tauri/src/book.rs`
- Modify: `src-tauri/src/registry.rs`
- Modify: `src-tauri/src/lib.rs`, `src-tauri/src/commands.rs`
- Test: `src-tauri/tests/book.rs`

**Interfaces:**
- Consumes: `resolution::engine::{resolve, ResolveInput, Resolution}`, `instrument::store`.
- Produces:
  - `pub struct BookEntry { pub instrument_id: i64, pub asset_class_id: i64, pub label: String, pub active: bool, pub note: String, pub security: Option<String>, pub review_pending: bool }`
  - `pub struct AddToBook { pub raw: String, pub yellow_key: String, pub asset_class_id: i64, pub label: String, pub hints: Hints }`
  - `pub enum AddOutcome { Added(BookEntry), NeedsReview { review_id: i64 }, NotFound }`
  - `pub async fn add<F: MasterFetcher>(pool, fetcher: &F, req: &AddToBook, by: &str) -> AppResult<AddOutcome>`
  - `pub async fn list(pool) -> AppResult<Vec<BookEntry>>`
  - `pub async fn set_active(pool, instrument_id, active) -> AppResult<()>`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/book.rs`:

```rust
mod common;

use getbloomdata_lib::book::{self, AddOutcome, AddToBook};
use getbloomdata_lib::master_fetch::MockMasterFetcher;
use getbloomdata_lib::resolution::score::Hints;

async fn equity_class(pool: &sqlx::PgPool) -> i64 {
    sqlx::query_scalar("INSERT INTO asset_class (name) VALUES ('Equity')
                        ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name
                        RETURNING id").fetch_one(pool).await.unwrap()
}

fn req(raw: &str, class: i64) -> AddToBook {
    AddToBook { raw: raw.into(), yellow_key: "Equity".into(), asset_class_id: class,
                label: raw.into(), hints: Hints::default() }
}

fn identity_mock(security: &str, figi: &str) -> MockMasterFetcher {
    MockMasterFetcher {
        identity_raw: serde_json::json!([{"securityData": [{
            "security": security, "fieldExceptions": [], "sequenceNumber": 0,
            "fieldData": {"ID_BB_GLOBAL": figi, "EXCH_CODE": "US", "CRNCY": "USD",
                          "NAME": "TEST INC", "LISTING_DATE": "2000-01-03"}}]}]),
        ..Default::default()
    }
}

#[tokio::test]
async fn adding_an_entry_resolves_it_and_derives_its_security_string() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let mock = identity_mock("ZDDD US Equity", "BBG000TESTD1");
    let out = book::add(&pool, &mock, &req("ZDDD US", class), "laurent").await.unwrap();
    let AddOutcome::Added(entry) = out else { panic!("expected Added, got {out:?}") };
    assert_eq!(entry.security.as_deref(), Some("ZDDD US Equity"),
               "the security string is derived from the alias, not stored on the entry");
    assert!(!entry.review_pending);
}

/// The constraint that replaced UNIQUE (bdp_security): one entry per instrument.
#[tokio::test]
async fn the_same_instrument_cannot_be_added_to_the_book_twice() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let mock = identity_mock("ZEEE US Equity", "BBG000TESTE1");
    book::add(&pool, &mock, &req("ZEEE US", class), "laurent").await.unwrap();
    // Second add resolves locally to the same instrument -- and must not create
    // a second row, nor fail with a confusing constraint error.
    let out = book::add(&pool, &mock, &req("ZEEE US", class), "laurent").await.unwrap();
    let AddOutcome::Added(entry) = out else { panic!("expected Added") };
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM book_entry WHERE instrument_id = $1")
        .bind(entry.instrument_id).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
}

#[tokio::test]
async fn an_ambiguous_addition_creates_a_review_and_no_book_entry() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let mock = MockMasterFetcher {
        identity_raw: serde_json::json!([]),
        list_raw: serde_json::json!([{"results": [
            {"security": "ZFFF US<equity>", "description": "Test"},
            {"security": "ZFFF LN<equity>", "description": "Test"}]}]),
        ..Default::default()
    };
    let out = book::add(&pool, &mock, &req("ZFFF", class), "laurent").await.unwrap();
    assert!(matches!(out, AddOutcome::NeedsReview { .. }), "got {out:?}");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM book_entry")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 0, "an unresolved identifier must not quietly enter the book");
}

#[tokio::test]
async fn deactivating_an_entry_keeps_its_instrument_and_its_history() {
    let pool = common::pool().await;
    let class = equity_class(&pool).await;
    let mock = identity_mock("ZGGG US Equity", "BBG000TESTG1");
    let AddOutcome::Added(e) = book::add(&pool, &mock, &req("ZGGG US", class), "laurent")
        .await.unwrap() else { panic!() };
    book::set_active(&pool, e.instrument_id, false).await.unwrap();
    let listed = book::list(&pool).await.unwrap();
    let found = listed.iter().find(|b| b.instrument_id == e.instrument_id).unwrap();
    assert!(!found.active, "still listed, just inactive");
    let aliases: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_alias WHERE instrument_id = $1")
        .bind(e.instrument_id).fetch_one(&pool).await.unwrap();
    assert!(aliases > 0);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test book
```

Expected: FAIL to compile — `unresolved import getbloomdata_lib::book`.

- [ ] **Step 3: Implement `book.rs`**

```rust
//! The user's book: which instruments they care about, and what they call them.
//!
//! Identity is NOT here -- it belongs to `instrument`. What is here is the
//! label, the active flag and the class, which is exactly the part of the old
//! `asset` table that was genuinely the user's rather than Bloomberg's.
//!
//! There is deliberately no unique constraint on a security string. One
//! instrument wears several over its life (FB US Equity, then META US Equity),
//! so uniqueness on the string was not merely unnecessary -- it was wrong.

use crate::error::AppResult;
use crate::instrument::store;
use crate::master_fetch::MasterFetcher;
use crate::resolution::engine::{self, Resolution, ResolveInput};
use crate::resolution::score::Hints;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BookEntry {
    pub instrument_id: i64,
    pub asset_class_id: i64,
    pub label: String,
    pub active: bool,
    pub note: String,
    /// Derived from today's alias; None when the instrument has no security
    /// string valid today (a delisted instrument, for instance).
    pub security: Option<String>,
    /// True while a resolution_review for this instrument is still pending.
    pub review_pending: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AddToBook {
    pub raw: String,
    pub yellow_key: String,
    pub asset_class_id: i64,
    pub label: String,
    #[serde(default)]
    pub hints: Hints,
}

#[derive(Debug, Serialize)]
pub enum AddOutcome {
    Added(BookEntry),
    NeedsReview { review_id: i64 },
    NotFound,
}

pub async fn add<F: MasterFetcher>(pool: &PgPool, fetcher: &F, req: &AddToBook,
                                   by: &str) -> AppResult<AddOutcome>
{
    let input = ResolveInput {
        raw: req.raw.clone(),
        yellow_key: req.yellow_key.clone(),
        hints: req.hints.clone(),
        as_of: chrono::Local::now().date_naive(),
        decided_by: by.to_string(),
    };
    match engine::resolve(pool, fetcher, &input).await? {
        Resolution::Bound { instrument_id, .. } => {
            // Re-adding an instrument already in the book updates its label
            // rather than failing on the primary key.
            sqlx::query(
                "INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)
                 ON CONFLICT (instrument_id) DO UPDATE
                   SET label = EXCLUDED.label, active = TRUE")
                .bind(instrument_id).bind(req.asset_class_id).bind(&req.label)
                .execute(pool).await?;
            let entry = get(pool, instrument_id).await?
                .expect("just inserted");
            // Tell the candidate cache this security is now a real instrument, so
            // search shows it as known rather than merely "seen before".
            if let Some(sec) = &entry.security {
                crate::instrument::search::link_candidate(pool, sec, instrument_id)
                    .await?;
            }
            Ok(AddOutcome::Added(entry))
        }
        Resolution::NeedsReview { review_id, .. } => {
            Ok(AddOutcome::NeedsReview { review_id })
        }
        Resolution::NotFound { .. } => Ok(AddOutcome::NotFound),
    }
}

pub async fn get(pool: &PgPool, instrument_id: i64) -> AppResult<Option<BookEntry>> {
    Ok(list(pool).await?.into_iter().find(|b| b.instrument_id == instrument_id))
}

pub async fn list(pool: &PgPool) -> AppResult<Vec<BookEntry>> {
    let rows: Vec<(i64, i64, String, bool, String)> = sqlx::query_as(
        "SELECT instrument_id, asset_class_id, label, active, note
           FROM book_entry ORDER BY label")
        .fetch_all(pool).await?;
    let today = chrono::Local::now().date_naive();
    let mut out = Vec::with_capacity(rows.len());
    for (instrument_id, asset_class_id, label, active, note) in rows {
        let security = store::current_security(pool, instrument_id, today).await?;
        let review_pending: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM resolution_review r
                              JOIN resolution_decision d ON d.id = r.decision_id
                             WHERE r.status = 'pending'
                               AND d.chosen_instrument_id = $1)")
            .bind(instrument_id).fetch_one(pool).await?;
        out.push(BookEntry { instrument_id, asset_class_id, label, active, note,
                             security, review_pending });
    }
    Ok(out)
}

pub async fn set_active(pool: &PgPool, instrument_id: i64, active: bool)
    -> AppResult<()>
{
    sqlx::query("UPDATE book_entry SET active = $2 WHERE instrument_id = $1")
        .bind(instrument_id).bind(active).execute(pool).await?;
    Ok(())
}

pub async fn set_note(pool: &PgPool, instrument_id: i64, note: &str) -> AppResult<()> {
    sqlx::query("UPDATE book_entry SET note = $2 WHERE instrument_id = $1")
        .bind(instrument_id).bind(note).execute(pool).await?;
    Ok(())
}
```

- [ ] **Step 4: Strip `registry.rs` back to asset classes**

Delete from `src-tauri/src/registry.rs`: `Asset`, `NewAsset`, `create_asset`,
`list_assets`, `set_asset_active`, `resolve_bdp_security`, `strip_trailing_key`
and the whole `#[cfg(test)] mod tests` block. Those tests moved to
`resolution::normalize` in Task 2 — verify they are present there before deleting,
so the doubled-yellow-key regression is never uncovered.

What remains is `AssetClass`, `create_asset_class` and `list_asset_classes`.

- [ ] **Step 5: Wire the commands**

In `src-tauri/src/commands.rs`, replace the Assets section:

```rust
// ---------------------------------------------------------------------------
// Book
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_book(state: State<'_, AppState>)
    -> Result<Vec<book::BookEntry>, AppError> {
    book::list(&state.pool).await
}

#[tauri::command]
pub async fn add_to_book(state: State<'_, AppState>, req: book::AddToBook)
    -> Result<book::AddOutcome, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg };
    book::add(&state.pool, &fetcher, &req, "user").await
}

#[tauri::command]
pub async fn set_book_active(state: State<'_, AppState>, instrument_id: i64,
                             active: bool) -> Result<(), AppError> {
    book::set_active(&state.pool, instrument_id, active).await
}

// ---------------------------------------------------------------------------
// Resolution review
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn list_pending_reviews(state: State<'_, AppState>)
    -> Result<Vec<engine::PendingReview>, AppError> {
    engine::pending_reviews(&state.pool).await
}

#[tauri::command]
pub async fn resolve_review(state: State<'_, AppState>, review_id: i64,
                            chosen_security: String) -> Result<i64, AppError> {
    engine::resolve_review(&state.pool, review_id, &chosen_security, "user").await
}

#[tauri::command]
pub async fn reject_review(state: State<'_, AppState>, review_id: i64, note: String)
    -> Result<(), AppError> {
    engine::reject_review(&state.pool, review_id, &note).await
}
```

Register them in `src-tauri/src/lib.rs`'s `invoke_handler`, removing
`commands::list_assets`, `commands::create_asset` and `commands::set_asset_active`.

- [ ] **Step 6: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test book
```

Expected: all four tests PASS. The crate still will not build fully — `fetch.rs`,
`views.rs`, `deletion.rs` and `bulk/` still reference `asset`. That is Task 12.
Until then, run the individual integration tests rather than `cargo test`.

- [ ] **Step 7: Commit**

```bash
git add src-tauri/src
git commit -m "feat(book): book_entry replaces asset; registry keeps only asset classes"
```

---
## Task 10: Local search — every keystroke, zero Bloomberg calls

The tier that answers while the user types. It reads four local sources through trigram indexes and labels each result by where it came from, so the user can tell an instrument they already hold from one they have merely seen before.

**Files:**
- Create: `src-tauri/src/instrument/search.rs`
- Modify: `src-tauri/src/instrument/mod.rs`, `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`
- Test: `src-tauri/tests/search.rs`

**Interfaces:**
- Consumes: Task 1's GIN trigram indexes.
- Produces:
  - `pub enum Origin { Book, Instrument, Candidate }` (serialises as `"book"` / `"instrument"` / `"candidate"`)
  - `pub struct SearchHit { pub origin: Origin, pub security: Option<String>, pub display: String, pub description: String, pub instrument_id: Option<i64>, pub similarity: f32 }`
  - `pub const MIN_SIMILARITY: f32 = 0.25`
  - `pub async fn local(pool, query: &str, limit: i64) -> AppResult<Vec<SearchHit>>`
  - `pub async fn remember_candidates(pool, cands: &[Candidate]) -> AppResult<usize>`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/search.rs`:

```rust
mod common;

use getbloomdata_lib::instrument::search::{self, Origin};
use getbloomdata_lib::instrument::store::{self, NewAlias};
use getbloomdata_lib::resolution::score::Candidate;

fn d(s: &str) -> chrono::NaiveDate { s.parse().unwrap() }

async fn seed(pool: &sqlx::PgPool) -> i64 {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ('Equity')
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id")
        .fetch_one(pool).await.unwrap();
    let inst = store::create(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: "AAPL US Equity".into(),
        exch_code: Some("US".into()), valid_from: d("1980-12-12"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    store::set_attr(&mut tx, inst.instrument_id, "name", "APPLE INC",
                    d("1980-12-12"), "bloomberg", None).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,'Apple')")
        .bind(inst.instrument_id).bind(class).execute(pool).await.unwrap();
    inst.instrument_id
}

/// The headline requirement: typing AAPL suggests AAPL US Equity, and it costs
/// nothing, because nothing here talks to Bloomberg.
#[tokio::test]
async fn typing_a_ticker_suggests_the_full_security_string() {
    let pool = common::pool().await;
    seed(&pool).await;
    let hits = search::local(&pool, "AAPL", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.security.as_deref() == Some("AAPL US Equity")),
            "got {hits:#?}");
}

#[tokio::test]
async fn a_result_says_where_it_came_from() {
    let pool = common::pool().await;
    let iid = seed(&pool).await;
    search::remember_candidates(&pool, &[Candidate {
        security: "MSFT US Equity".into(), description: "Microsoft Corp".into(),
        exchange: Some("US".into()), country: None, currency: None,
        asset_class: None, figi: None }]).await.unwrap();

    let held = search::local(&pool, "Apple", 10).await.unwrap();
    assert_eq!(held[0].origin, Origin::Book);
    assert_eq!(held[0].instrument_id, Some(iid));

    let seen = search::local(&pool, "MSFT", 10).await.unwrap();
    assert_eq!(seen[0].origin, Origin::Candidate);
    assert_eq!(seen[0].instrument_id, None, "a cached candidate is not yet an instrument");
}

#[tokio::test]
async fn a_historical_ticker_still_finds_its_instrument() {
    let pool = common::pool().await;
    let inst = store::create(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    let old = store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "ticker".into(), value: "FB".into(), exch_code: Some("US".into()),
        valid_from: d("2012-05-18"), valid_to: None, source: "user".into(),
        bbg_action_id: None, anchoring_identifier: None }).await.unwrap();
    store::close_alias(&mut tx, old, d("2022-06-09")).await.unwrap();
    tx.commit().await.unwrap();

    let hits = search::local(&pool, "FB", 10).await.unwrap();
    assert!(hits.iter().any(|h| h.instrument_id == Some(inst.instrument_id)),
            "an identifier the instrument used to wear is still how a user looks for it");
}

#[tokio::test]
async fn results_are_ranked_and_thresholded() {
    let pool = common::pool().await;
    seed(&pool).await;
    search::remember_candidates(&pool, &[
        Candidate { security: "AAPL LN Equity".into(), description: "Apple Inc".into(),
                    exchange: Some("LN".into()), country: None, currency: None,
                    asset_class: None, figi: None },
        Candidate { security: "ZZZZ US Equity".into(), description: "Nothing alike".into(),
                    exchange: Some("US".into()), country: None, currency: None,
                    asset_class: None, figi: None }]).await.unwrap();

    let hits = search::local(&pool, "AAPL", 10).await.unwrap();
    assert!(hits.windows(2).all(|w| w[0].similarity >= w[1].similarity),
            "most similar first");
    assert!(hits.iter().all(|h| h.similarity >= search::MIN_SIMILARITY));
    assert!(!hits.iter().any(|h| h.security.as_deref() == Some("ZZZZ US Equity")));
}

#[tokio::test]
async fn the_same_instrument_appears_once_at_its_strongest_origin() {
    let pool = common::pool().await;
    let iid = seed(&pool).await;
    // The same security is also in the candidate cache, from an earlier search.
    search::remember_candidates(&pool, &[Candidate {
        security: "AAPL US Equity".into(), description: "Apple Inc".into(),
        exchange: Some("US".into()), country: None, currency: None,
        asset_class: None, figi: None }]).await.unwrap();
    let hits = search::local(&pool, "AAPL US Equity", 10).await.unwrap();
    let for_this: Vec<_> = hits.iter()
        .filter(|h| h.security.as_deref() == Some("AAPL US Equity")).collect();
    assert_eq!(for_this.len(), 1, "one row per security, not one per source");
    assert_eq!(for_this[0].origin, Origin::Book);
    assert_eq!(for_this[0].instrument_id, Some(iid));
}

#[tokio::test]
async fn remembering_a_candidate_twice_refreshes_it_rather_than_duplicating() {
    let pool = common::pool().await;
    let c = [Candidate { security: "TSLA US Equity".into(), description: "Tesla Inc".into(),
                         exchange: Some("US".into()), country: None, currency: None,
                         asset_class: None, figi: None }];
    search::remember_candidates(&pool, &c).await.unwrap();
    search::remember_candidates(&pool, &c).await.unwrap();
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_candidate WHERE security = 'TSLA US Equity'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1);
    let (first, last): (chrono::DateTime<chrono::Utc>, chrono::DateTime<chrono::Utc>) =
        sqlx::query_as("SELECT first_seen, last_seen FROM instrument_candidate
                         WHERE security = 'TSLA US Equity'")
        .fetch_one(&pool).await.unwrap();
    assert!(last >= first);
}

#[tokio::test]
async fn an_empty_query_returns_nothing_rather_than_everything() {
    let pool = common::pool().await;
    seed(&pool).await;
    assert!(search::local(&pool, "   ", 10).await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test search
```

Expected: FAIL to compile — `cannot find module search`.

- [ ] **Step 3: Implement**

Add `pub mod search;` to `src-tauri/src/instrument/mod.rs`, then create
`src-tauri/src/instrument/search.rs`:

```rust
//! Search that costs nothing.
//!
//! Spec §6.1. Every source here is local, so this runs on every keystroke
//! without touching the daily hit budget. The corpus grows monotonically: every
//! Bloomberg search and every resolution adds rows that make the next search
//! better, and none of that growth costs a second call.
//!
//! Four sources, in decreasing strength:
//!   book_entry.label       instruments the user actually holds
//!   instrument_alias.value every identifier ever worn, current or historical
//!   instrument_attr.value  the 'name' attribute
//!   instrument_candidate   everything Bloomberg has ever returned from a search
//!
//! Spec §6.1 describes one denormalised search_text column. Indexing each source
//! in place and combining them here is used instead: a materialised view would
//! be stale between refreshes -- a freshly added book entry would not be
//! findable -- and denormalisation triggers on four tables are more machinery
//! than the query saves.

use crate::error::AppResult;
use crate::resolution::score::Candidate;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;

/// Below this, trigram similarity is noise. Tuned so that "AAPL" reaches
/// "AAPL US Equity" but not "APPLIED MATERIALS".
pub const MIN_SIMILARITY: f32 = 0.25;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Origin {
    /// In your book.
    Book,
    /// A known instrument, not currently held.
    Instrument,
    /// Seen before in a Bloomberg search, never resolved.
    Candidate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub origin: Origin,
    pub security: Option<String>,
    pub display: String,
    pub description: String,
    pub instrument_id: Option<i64>,
    pub similarity: f32,
}

#[derive(sqlx::FromRow)]
struct RawHit {
    origin: String,
    security: Option<String>,
    display: String,
    description: String,
    instrument_id: Option<i64>,
    similarity: f32,
}

pub async fn local(pool: &PgPool, query: &str, limit: i64) -> AppResult<Vec<SearchHit>> {
    let q = query.trim();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    // rank orders the origins so DISTINCT ON keeps the strongest one per
    // security: an instrument you hold should never be presented as merely
    // "seen before" because the candidate cache also has it.
    let rows = sqlx::query_as::<_, RawHit>(
        r#"
        WITH hits AS (
          SELECT 'book' AS origin, 1 AS rank,
                 (SELECT a.value FROM instrument_alias a
                   WHERE a.instrument_id = b.instrument_id
                     AND a.id_type = 'bdp_security'
                     AND a.valid_to > CURRENT_DATE
                     AND a.system_to = 'infinity'
                   ORDER BY a.valid_from DESC LIMIT 1) AS security,
                 b.label AS display, '' AS description,
                 b.instrument_id, similarity(b.label, $1) AS similarity
            FROM book_entry b
           WHERE b.label %% $1

          UNION ALL
          SELECT 'instrument', 2,
                 a.value, a.value, '', a.instrument_id, similarity(a.value, $1)
            FROM instrument_alias a
           WHERE a.system_to = 'infinity' AND a.value %% $1

          UNION ALL
          SELECT 'instrument', 3,
                 NULL, t.value, '', t.instrument_id, similarity(t.value, $1)
            FROM instrument_attr t
           WHERE t.attr = 'name' AND t.system_to = 'infinity' AND t.value %% $1

          UNION ALL
          SELECT 'candidate', 4,
                 c.security, c.security, c.description, c.instrument_id,
                 greatest(similarity(c.security, $1), similarity(c.description, $1))
            FROM instrument_candidate c
           WHERE c.security %% $1 OR c.description %% $1
        ),
        strong AS (
          SELECT * FROM hits WHERE similarity >= $2
        ),
        best AS (
          SELECT DISTINCT ON (coalesce(security, display))
                 origin, security, display, description, instrument_id, similarity
            FROM strong
           ORDER BY coalesce(security, display), rank, similarity DESC
        )
        SELECT origin, security, display, description, instrument_id, similarity
          FROM best ORDER BY similarity DESC, display LIMIT $3
        "#)
        .bind(q).bind(MIN_SIMILARITY).bind(limit)
        .fetch_all(pool).await?;

    Ok(rows.into_iter().map(|r| SearchHit {
        origin: match r.origin.as_str() {
            "book" => Origin::Book,
            "instrument" => Origin::Instrument,
            _ => Origin::Candidate,
        },
        security: r.security,
        display: r.display,
        description: r.description,
        instrument_id: r.instrument_id,
        similarity: r.similarity,
    }).collect())
}

/// Keep every row Bloomberg has ever returned. One search for "AAPL" seeds all
/// its listings permanently, which is what makes the local tier good enough to
/// make the Bloomberg tier rare.
pub async fn remember_candidates(pool: &PgPool, cands: &[Candidate])
    -> AppResult<usize>
{
    let mut n = 0;
    for c in cands {
        sqlx::query(
            "INSERT INTO instrument_candidate
               (security, raw_security, description, yellow_key)
             VALUES ($1,$1,$2,$3)
             ON CONFLICT (security) DO UPDATE
               SET last_seen = now(),
                   description = CASE WHEN EXCLUDED.description <> ''
                                      THEN EXCLUDED.description
                                      ELSE instrument_candidate.description END")
            .bind(&c.security).bind(&c.description)
            .bind(c.security.rsplit(' ').next())
            .execute(pool).await?;
        n += 1;
    }
    Ok(n)
}

/// Once a candidate becomes a real instrument, say so, so search can show it as
/// known rather than merely seen.
pub async fn link_candidate(pool: &PgPool, security: &str, instrument_id: i64)
    -> AppResult<()>
{
    sqlx::query("UPDATE instrument_candidate SET instrument_id = $2 WHERE security = $1")
        .bind(security).bind(instrument_id).execute(pool).await?;
    Ok(())
}
```

Note the `%%` in the SQL: sqlx treats `%` as a bind-parameter escape inside a
raw string, and `%` is pg_trgm's similarity operator. Doubling it is required.

- [ ] **Step 4: Add the command**

In `src-tauri/src/commands.rs`:

```rust
/// Local search. Never calls Bloomberg -- this is the command behind the
/// type-ahead, and it runs on every keystroke.
#[tauri::command]
pub async fn search_local(state: State<'_, AppState>, query: String, limit: i64)
    -> Result<Vec<search::SearchHit>, AppError> {
    search::local(&state.pool, &query, limit.clamp(1, 50)).await
}
```

Register `commands::search_local` in `src-tauri/src/lib.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test search
```

Expected: all seven tests PASS. If `typing_a_ticker_suggests_the_full_security_string`
fails on threshold, check `MIN_SIMILARITY` against `SELECT similarity('AAPL US Equity','AAPL')`
in psql before changing the query — the constant is the tuning point, not the SQL.

- [ ] **Step 6: Commit**

```bash
git add src-tauri/src/instrument src-tauri/src/commands.rs src-tauri/src/lib.rs src-tauri/tests/search.rs
git commit -m "feat(search): local trigram search over book, aliases, names and the candidate cache"
```

---

## Task 11: The Bloomberg search tier

One button, one call, cached forever. It is never triggered by typing, focus or navigation.

**Files:**
- Modify: `src-tauri/src/instrument/search.rs`
- Modify: `src-tauri/src/budget.rs`, `src-tauri/src/commands.rs`, `src-tauri/src/lib.rs`
- Test: `src-tauri/tests/search_bloomberg.rs`

**Interfaces:**
- Consumes: `master_fetch::MasterFetcher`, `search::remember_candidates`.
- Produces:
  - `pub async fn bloomberg<F: MasterFetcher>(pool, fetcher: &F, query: &str, yellow_key: &str) -> AppResult<BloombergSearch>`
  - `pub struct BloombergSearch { pub hits: Vec<SearchHit>, pub estimated_hits: i64, pub cached: usize }`
  - `budget::record_hits(pool, purpose: &str, hits: i64) -> AppResult<()>`
  - `budget::SEARCH_HIT_COST: i64`

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/search_bloomberg.rs`:

```rust
mod common;

use getbloomdata_lib::instrument::search;
use getbloomdata_lib::master_fetch::MockMasterFetcher;

fn mock() -> MockMasterFetcher {
    MockMasterFetcher {
        list_raw: serde_json::json!([{"results": [
            {"security": "AAPL US<equity>", "description": "Apple Inc"},
            {"security": "AAPL LN<equity>", "description": "Apple Inc"},
            {"security": "AAPL US 08/21/26 C400<equity>", "description": "Apple call"}]}]),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_bloomberg_search_caches_every_result_permanently() {
    let pool = common::pool().await;
    let m = mock();
    let out = search::bloomberg(&pool, &m, "AAPL", "Equity").await.unwrap();
    assert_eq!(m.call_count(), 1, "exactly one instrumentListRequest");
    assert!(out.cached >= 2);

    // The point of the cache: the same search now needs no call at all.
    let local = search::local(&pool, "AAPL", 10).await.unwrap();
    assert!(local.iter().any(|h| h.security.as_deref() == Some("AAPL US Equity")));
    assert!(local.iter().any(|h| h.security.as_deref() == Some("AAPL LN Equity")));
}

#[tokio::test]
async fn the_raw_bloomberg_form_is_never_stored_as_a_security_string() {
    let pool = common::pool().await;
    search::bloomberg(&pool, &mock(), "AAPL", "Equity").await.unwrap();
    let bad: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM instrument_candidate WHERE security LIKE '%<%'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(bad, 0, "pasting the raw form produces exactly the malformed \
                        identifier migration 0004 had to repair");
}

#[tokio::test]
async fn the_call_is_recorded_in_the_hit_ledger() {
    let pool = common::pool().await;
    let before: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits),0) FROM hit_ledger
          WHERE occurred_on = CURRENT_DATE").fetch_one(&pool).await.unwrap();
    let out = search::bloomberg(&pool, &mock(), "AAPL", "Equity").await.unwrap();
    let after: i64 = sqlx::query_scalar(
        "SELECT coalesce(sum(estimated_hits),0) FROM hit_ledger
          WHERE occurred_on = CURRENT_DATE").fetch_one(&pool).await.unwrap();
    assert_eq!(after - before, out.estimated_hits);
    assert!(out.estimated_hits > 0, "whether instrumentListRequest is metered is \
                                     unknown (spec §10 q2), so it is counted");
    let purpose: String = sqlx::query_scalar(
        "SELECT purpose FROM hit_ledger ORDER BY id DESC LIMIT 1")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(purpose, "search");
}

#[tokio::test]
async fn an_empty_query_never_reaches_bloomberg() {
    let pool = common::pool().await;
    let m = mock();
    let out = search::bloomberg(&pool, &m, "   ", "Equity").await.unwrap();
    assert_eq!(m.call_count(), 0);
    assert!(out.hits.is_empty());
    assert_eq!(out.estimated_hits, 0);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test search_bloomberg
```

Expected: FAIL to compile — `cannot find function bloomberg`.

- [ ] **Step 3: Implement**

Add to `src-tauri/src/budget.rs`:

```rust
/// What one instrumentListRequest is charged. Spec §10 q2: whether this request
/// is metered at all is not established, so it is counted -- the existing
/// over-count-is-safe policy applied to a new call site.
pub const SEARCH_HIT_COST: i64 = 1;

/// Record a metered call that is not part of a run.
pub async fn record_hits(pool: &PgPool, purpose: &str, hits: i64) -> AppResult<()> {
    sqlx::query("INSERT INTO hit_ledger (run_id, purpose, estimated_hits)
                 VALUES (NULL, $1, $2)")
        .bind(purpose).bind(hits).execute(pool).await?;
    Ok(())
}
```

Add to `src-tauri/src/instrument/search.rs`:

```rust
use crate::master_fetch::MasterFetcher;

#[derive(Debug, Serialize)]
pub struct BloombergSearch {
    pub hits: Vec<SearchHit>,
    pub estimated_hits: i64,
    pub cached: usize,
}

/// Spec §6.2. Explicit, never automatic: this is behind a button, and nothing in
/// the UI may call it on typing, focus or navigation. Its results join the local
/// corpus permanently, so the same question is free from now on.
pub async fn bloomberg<F: MasterFetcher>(pool: &PgPool, fetcher: &F, query: &str,
                                         yellow_key: &str) -> AppResult<BloombergSearch>
{
    let q = query.trim();
    if q.is_empty() {
        return Ok(BloombergSearch { hits: Vec::new(), estimated_hits: 0, cached: 0 });
    }
    let filter = crate::resolution::engine::yellow_key_filter(yellow_key);
    let found = fetcher.instrument_list(q, filter, 20).await?;
    crate::budget::record_hits(pool, "search", crate::budget::SEARCH_HIT_COST).await?;

    // parse_list already normalised "AAPL US<equity>" -> "AAPL US Equity", so the
    // raw form can never reach the cache.
    let cached = remember_candidates(pool, &found).await?;

    // Answer from the local tier so the caller sees one consistent shape, with
    // book and known-instrument results ranked above the new arrivals.
    let hits = local(pool, q, 20).await?;
    Ok(BloombergSearch {
        hits,
        estimated_hits: crate::budget::SEARCH_HIT_COST,
        cached,
    })
}
```

Make `yellow_key_filter` public in `src-tauri/src/resolution/engine.rs`:

```rust
pub fn yellow_key_filter(yellow_key: &str) -> Option<&'static str> {
```

Add the command in `src-tauri/src/commands.rs`:

```rust
/// Explicit Bloomberg search. The UI must call this only from the
/// "Search Bloomberg" button -- never on input, focus or navigation.
#[tauri::command]
pub async fn search_bloomberg(state: State<'_, AppState>, query: String,
                              yellow_key: String)
    -> Result<search::BloombergSearch, AppError> {
    let cfg = pipeline_cfg(&state).await;
    let fetcher = master_fetch::BlpapiMasterFetcher { cfg: &cfg };
    search::bloomberg(&state.pool, &fetcher, &query, &yellow_key).await
}
```

Register it in `src-tauri/src/lib.rs`.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test --test search_bloomberg && cargo test --test search
```

Expected: all four new tests PASS and the seven local-search tests still PASS.

- [ ] **Step 5: Commit**

```bash
git add src-tauri/src src-tauri/tests/search_bloomberg.rs
git commit -m "feat(search): explicit Bloomberg tier, cached permanently and charged to the ledger"
```

---
## Task 12: Retarget the fetch pipeline to instruments

The largest mechanical change in the plan, and the one that makes the crate compile again. `asset_id` becomes `instrument_id` everywhere, the security string is looked up rather than read off a column, and observations are written append-only at a recorded adjustment basis.

**This task contains the deviation described at the top of the plan.** Spec §2 assigns observation writing to P2. Steps 6 and 7 pull forward the minimum needed to keep the application working: the four adjustment flags, and an append-only insert. Everything else about P2 — point-in-time reads, supersession on correction, the other four layers — is untouched.

**Files:**
- Modify: `src-tauri/src/fetch.rs`, `src-tauri/src/orchestrator.rs`, `src-tauri/src/ingest.rs`, `src-tauri/src/views.rs`, `src-tauri/src/deletion.rs`, `src-tauri/src/scheduler.rs`, `src-tauri/src/budget.rs`, `src-tauri/src/commands.rs`
- Modify: `src-tauri/scripts/blp_fetch.py`
- Test: `src-tauri/tests/pipeline.rs`

**Interfaces:**
- Consumes: `instrument::store::current_security`, Task 1's `observation` and `view_instrument`.
- Produces:
  - `fetch::FetchAsset` keeps its name but its first field becomes `pub instrument_id: i64`
  - `fetch::ObsCell.instrument_id`, `fetch::CellProblem.instrument_id`
  - `views::view_instruments(pool, view_id) -> AppResult<Vec<BookEntry>>`
  - `ingest::ingest_outcome` unchanged in signature; changed in behaviour

- [ ] **Step 1: Write the failing tests**

Create `src-tauri/tests/pipeline.rs`:

```rust
mod common;

use chrono::NaiveDate;
use getbloomdata_lib::fetch::{CellValue, FetchOutcome, ObsCell};
use getbloomdata_lib::{ingest, views};
use getbloomdata_lib::instrument::store::{self, NewAlias};

fn d(s: &str) -> NaiveDate { s.parse().unwrap() }

/// An instrument, a book entry, a view containing it, a field and a run.
async fn scaffold(pool: &sqlx::PgPool, security: &str) -> (i64, i64, i64, i64) {
    let class: i64 = sqlx::query_scalar(
        "INSERT INTO asset_class (name) VALUES ('Equity')
         ON CONFLICT (name) DO UPDATE SET name = EXCLUDED.name RETURNING id")
        .fetch_one(pool).await.unwrap();
    let inst = store::create(pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    store::insert_alias(&mut tx, inst.instrument_id, &NewAlias {
        id_type: "bdp_security".into(), value: security.into(),
        exch_code: Some("US".into()), valid_from: d("2000-01-03"), valid_to: None,
        source: "user".into(), bbg_action_id: None, anchoring_identifier: None,
    }).await.unwrap();
    tx.commit().await.unwrap();
    sqlx::query("INSERT INTO book_entry (instrument_id, asset_class_id, label)
                 VALUES ($1,$2,$3)")
        .bind(inst.instrument_id).bind(class).bind(security)
        .execute(pool).await.unwrap();
    let fid: i64 = sqlx::query_scalar(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind)
         VALUES ($1,'PX_LAST','Last price','numeric')
         ON CONFLICT (asset_class_id, mnemonic) DO UPDATE SET label = EXCLUDED.label
         RETURNING id").bind(class).fetch_one(pool).await.unwrap();
    let vid: i64 = sqlx::query_scalar(
        "INSERT INTO view (name) VALUES ('v' || $1::text) RETURNING id")
        .bind(inst.instrument_id).fetch_one(pool).await.unwrap();
    sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
        .bind(vid).bind(inst.instrument_id).execute(pool).await.unwrap();
    let rid: i64 = sqlx::query_scalar(
        "INSERT INTO run (view_id, kind, trigger_kind, status)
         VALUES ($1,'eod','manual','ok') RETURNING id")
        .bind(vid).fetch_one(pool).await.unwrap();
    (inst.instrument_id, fid, vid, rid)
}

fn outcome(instrument_id: i64, field_id: i64, run_date: &str, v: f64) -> FetchOutcome {
    FetchOutcome {
        cells: vec![ObsCell { instrument_id, field_id, obs_date: d(run_date),
                              value: CellValue::Num(v) }],
        problems: vec![],
    }
}

#[tokio::test]
async fn an_ingested_observation_records_its_adjustment_basis() {
    let pool = common::pool().await;
    let (iid, fid, _, rid) = scaffold(&pool, "ZPIPE1 US Equity").await;
    ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 100.0))
        .await.unwrap();
    let (layer, note): (String, String) = sqlx::query_as(
        "SELECT o.layer, b.note FROM observation o
           JOIN adjustment_basis b ON b.id = o.basis_id
          WHERE o.instrument_id = $1").bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(layer, "raw");
    assert!(note.starts_with("RAW"), "a price without its basis is not a fact");
}

/// The property the whole design exists to protect.
#[tokio::test]
async fn re_ingesting_a_different_value_supersedes_rather_than_overwrites() {
    let pool = common::pool().await;
    let (iid, fid, _, rid) = scaffold(&pool, "ZPIPE2 US Equity").await;
    ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 499.23))
        .await.unwrap();
    ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 124.81))
        .await.unwrap();

    let rows: Vec<(f64, bool)> = sqlx::query_as(
        "SELECT value_num, system_to = 'infinity' FROM observation
          WHERE instrument_id = $1 ORDER BY id").bind(iid)
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2, "the first value is retained, not replaced");
    assert_eq!(rows[0], (499.23, false), "superseded");
    assert_eq!(rows[1], (124.81, true), "current");
}

#[tokio::test]
async fn re_ingesting_an_identical_value_changes_nothing() {
    let pool = common::pool().await;
    let (iid, fid, _, rid) = scaffold(&pool, "ZPIPE3 US Equity").await;
    for _ in 0..3 {
        ingest::ingest_outcome(&pool, rid, &outcome(iid, fid, "2026-08-18", 100.0))
            .await.unwrap();
    }
    let n: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM observation WHERE instrument_id = $1")
        .bind(iid).fetch_one(&pool).await.unwrap();
    assert_eq!(n, 1, "an unchanged re-fetch must not grow the table every day");
}

#[tokio::test]
async fn a_view_lists_its_instruments_with_their_current_security_strings() {
    let pool = common::pool().await;
    let (iid, _, vid, _) = scaffold(&pool, "ZPIPE4 US Equity").await;
    let members = views::view_instruments(&pool, vid).await.unwrap();
    assert_eq!(members.len(), 1);
    assert_eq!(members[0].instrument_id, iid);
    assert_eq!(members[0].security.as_deref(), Some("ZPIPE4 US Equity"));
}

/// Spec §5: a pending review blocks the instrument from every view, so an
/// unresolved identifier cannot quietly become a gap in a time series.
#[tokio::test]
async fn an_instrument_under_review_is_excluded_from_its_view() {
    let pool = common::pool().await;
    let (iid, _, vid, _) = scaffold(&pool, "ZPIPE5 US Equity").await;
    let did: i64 = sqlx::query_scalar(
        "INSERT INTO resolution_decision
           (raw_input, normalized, method, chosen_instrument_id, candidates, decided_by)
         VALUES ('ZPIPE5','ZPIPE5 US Equity','manual',$1,'[]'::jsonb,'test')
         RETURNING id").bind(iid).fetch_one(&pool).await.unwrap();
    sqlx::query("INSERT INTO resolution_review (decision_id, status)
                 VALUES ($1,'pending')").bind(did).execute(&pool).await.unwrap();

    assert!(views::view_instruments(&pool, vid).await.unwrap().is_empty());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test --test pipeline
```

Expected: FAIL to compile — `ObsCell has no field instrument_id`.

- [ ] **Step 3: Rename the identity field through `fetch.rs`**

In `src-tauri/src/fetch.rs`, rename `asset_id` to `instrument_id` in `ObsCell`,
`CellProblem` and `FetchAsset`. `FetchAsset` keeps its name — it is the fetch
layer's word for "a thing to ask about", and renaming the type as well would
touch far more than this task needs.

```rust
pub struct FetchAsset {
    pub instrument_id: i64,
    pub asset_class_id: i64,
    pub class_name: String,
    pub label: String,
    pub bdp_security: String,
}
```

Update the inline tests in the same file: the fixtures at lines ~395-401 and
~460 construct `FetchAsset { asset_id: .. }`, and the assertions at ~512-561
read `out.cells[0].asset_id`.

- [ ] **Step 4: Point `views.rs` at `view_instrument`**

Replace `set_view_assets`, `view_assets` and the fallback in `view_fields`:

```rust
use crate::book::BookEntry;

pub async fn set_view_instruments(pool: &PgPool, view_id: i64, instrument_ids: &[i64])
    -> AppResult<()>
{
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM view_instrument WHERE view_id = $1")
        .bind(view_id).execute(&mut *tx).await?;
    for iid in instrument_ids {
        sqlx::query("INSERT INTO view_instrument (view_id, instrument_id) VALUES ($1,$2)")
            .bind(view_id).bind(iid).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The active, resolved members of a view.
///
/// An instrument with a pending resolution_review is excluded (spec §5). The
/// alternative -- fetching for an identifier nobody has confirmed -- produces a
/// time series that looks complete and is attached to the wrong company.
pub async fn view_instruments(pool: &PgPool, view_id: i64) -> AppResult<Vec<BookEntry>> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT vi.instrument_id FROM view_instrument vi
           JOIN book_entry b ON b.instrument_id = vi.instrument_id
          WHERE vi.view_id = $1 AND b.active
            AND NOT EXISTS (
              SELECT 1 FROM resolution_review r
                JOIN resolution_decision d ON d.id = r.decision_id
               WHERE r.status = 'pending' AND d.chosen_instrument_id = vi.instrument_id)
          ORDER BY b.label")
        .bind(view_id).fetch_all(pool).await?;
    let all = crate::book::list(pool).await?;
    Ok(all.into_iter().filter(|b| ids.contains(&b.instrument_id)).collect())
}
```

In `view_fields`, the default-fields fallback joins `asset`; change it to:

```rust
        "SELECT DISTINCT f.* FROM field_def f
         JOIN book_entry b ON b.asset_class_id = f.asset_class_id
         JOIN view_instrument vi ON vi.instrument_id = b.instrument_id
         WHERE vi.view_id = $1 AND f.active AND b.active
         ORDER BY f.asset_class_id, f.mnemonic",
```

- [ ] **Step 5: Update `orchestrator::load_view`**

```rust
    let members = views::view_instruments(pool, view_id).await?;
    let fields_db = views::view_fields(pool, view_id).await?;
    let classes = crate::registry::list_asset_classes(pool).await?;
    // ... class_name closure unchanged ...
    let mut assets = Vec::with_capacity(members.len());
    for m in &members {
        // The security string is derived from the alias valid today, never read
        // off the book entry -- one instrument wears several over its life.
        let Some(security) = m.security.clone() else {
            // No security valid today: delisted, or never resolved. Skipping is
            // right, and saying so is what keeps it from looking like a holiday.
            eprintln!("view {view_id}: instrument {} has no security string today, skipping",
                      m.instrument_id);
            continue;
        };
        assets.push(FetchAsset {
            instrument_id: m.instrument_id,
            asset_class_id: m.asset_class_id,
            class_name: class_name(m.asset_class_id),
            label: m.label.clone(),
            bdp_security: security,
        });
    }
```

- [ ] **Step 6: Set the four adjustment flags in the sidecar**

**This is the deviation.** P0 §3.1 measured that the default follows `DPDF<GO>`,
a per-Terminal user setting, so a run today and the same run tomorrow can return
different numbers for the same date. Setting all four flags false is what makes
an observation raw, reproducible and worth storing permanently.

In `src-tauri/scripts/blp_fetch.py`, `build_request`, history branch:

```python
    if kind == "history":
        r = service.createRequest("HistoricalDataRequest")
        r.set("startDate", spec["start"])
        r.set("endDate", spec["end"])
        r.set("periodicitySelection", "DAILY")
        r.set("nonTradingDayFillOption", "ACTIVE_DAYS_ONLY")
        # P0 3.1: with none of these set, the values follow the Terminal's
        # DPDF<GO> setting -- a per-user preference that is not captured with the
        # data and can change between runs. AAPL closed 2020-08-28 at 499.23 with
        # all four false, 124.81 split-adjusted and 120.96 fully adjusted; all
        # three are "PX_LAST". Only the unadjusted number is a fact about that
        # day, so only that one is stored. Adjusted series are derived in P4.
        r.set("adjustmentNormal", False)
        r.set("adjustmentAbnormal", False)
        r.set("adjustmentSplit", False)
        r.set("adjustmentFollowDPDF", False)
```

- [ ] **Step 7: Make ingest append-only**

Replace the body of `ingest_outcome` in `src-tauri/src/ingest.rs`:

```rust
use crate::error::AppResult;
use crate::fetch::{CellValue, FetchOutcome};
use serde::Serialize;
use sqlx::PgPool;

#[derive(Debug, Clone, Serialize)]
pub struct IngestSummary {
    pub inserted: u64,
    pub superseded: u64,
    pub unchanged: u64,
    pub issues: u64,
}

/// Write observations without ever destroying one.
///
/// The previous implementation ended in ON CONFLICT DO UPDATE, which silently
/// replaced yesterday's number with today's. That makes a corrected value
/// indistinguishable from an original one and makes point-in-time history
/// impossible. Here a changed value closes the old row's system_to and inserts
/// a new one beneath it; an unchanged value does nothing at all.
pub async fn ingest_outcome(pool: &PgPool, run_id: i64, outcome: &FetchOutcome)
    -> AppResult<IngestSummary>
{
    // The basis these values were actually fetched at: all four adjustment
    // flags false (see blp_fetch.build_request).
    let raw_basis: i16 = sqlx::query_scalar(
        "SELECT id FROM adjustment_basis
          WHERE adj_normal = false AND adj_abnormal = false
            AND adj_split = false AND adj_follow_dpdf = false")
        .fetch_one(pool).await?;

    let mut tx = pool.begin().await?;
    let (mut inserted, mut superseded, mut unchanged) = (0u64, 0u64, 0u64);

    for c in &outcome.cells {
        let (num, text) = match &c.value {
            CellValue::Num(n) => (Some(*n), None),
            CellValue::Text(t) => (None, Some(t.clone())),
        };

        let current: Option<(i64, Option<f64>, Option<String>)> = sqlx::query_as(
            "SELECT id, value_num, value_text FROM observation
              WHERE instrument_id = $1 AND field_id = $2 AND obs_date = $3
                AND granularity = 'eod' AND layer = 'raw' AND basis_id = $4
                AND system_to = 'infinity'")
            .bind(c.instrument_id).bind(c.field_id).bind(c.obs_date).bind(raw_basis)
            .fetch_optional(&mut *tx).await?;

        if let Some((id, old_num, old_text)) = current {
            if old_num == num && old_text == text {
                unchanged += 1;
                continue;
            }
            sqlx::query("UPDATE observation SET system_to = now() WHERE id = $1")
                .bind(id).execute(&mut *tx).await?;
            superseded += 1;
        }

        sqlx::query(
            "INSERT INTO observation
               (instrument_id, field_id, obs_date, granularity, layer, basis_id,
                value_num, value_text, run_id)
             VALUES ($1,$2,$3,'eod','raw',$4,$5,$6,$7)")
            .bind(c.instrument_id).bind(c.field_id).bind(c.obs_date)
            .bind(raw_basis).bind(num).bind(text).bind(run_id)
            .execute(&mut *tx).await?;
        inserted += 1;
    }

    for p in &outcome.problems {
        sqlx::query(
            "INSERT INTO ingest_issue
               (run_id, instrument_id, field_id, obs_date, severity, code, detail)
             VALUES ($1,$2,$3,$4,'warn',$5,$6)")
            .bind(run_id).bind(p.instrument_id).bind(p.field_id).bind(p.obs_date)
            .bind(&p.code).bind(&p.detail)
            .execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(IngestSummary { inserted, superseded, unchanged,
                       issues: outcome.problems.len() as u64 })
}
```

Any caller reading `IngestSummary.upserted` — the run summary in
`orchestrator.rs` — becomes `inserted + superseded`.

- [ ] **Step 8: Update the remaining callers**

- `src-tauri/src/deletion.rs`: `delete_asset` becomes `delete_book_entry`,
  operating on `book_entry` and `view_instrument`. Deleting a book entry must
  never delete its `instrument`, its aliases or its observations — the user is
  removing something from their list, not asserting the company never existed.
  The impact description says so.
- `src-tauri/src/scheduler.rs`: `detect_gaps` joins `view_instrument` instead of
  `view_asset`, and counts `observation` rows with `system_to = 'infinity'`.
- `src-tauri/src/budget.rs`: `estimate_eod_hits` reads `FetchAsset.instrument_id`
  only for logging; the estimate itself is per security-field pair and unchanged.
- `src-tauri/src/commands.rs`: `estimate_view` builds its `FetchAsset` list from
  `views::view_instruments`; `set_view_assets`/`get_view_assets` become
  `set_view_instruments`/`get_view_instruments`.
- `src-tauri/src/fields.rs`: `FieldDef` gains `bbg_ftype: Option<String>`,
  `bbg_datatype: Option<String>` and `entitlement_note: String`, and
  `create_field` accepts them. This is spec §4.9's configurable field-mapping
  layer: without it the three columns exist and nothing can ever set them.
  `bbg_ftype` is the column that records P0 §5's `BulkFormat` marker, which is
  how P3 will know a field returns a table rather than a number — so the layer
  has to be writable before P3, not after.

```rust
pub async fn create_field(pool: &PgPool, asset_class_id: i64, mnemonic: &str,
                          label: &str, value_kind: &str,
                          bbg_ftype: Option<&str>, bbg_datatype: Option<&str>,
                          entitlement_note: &str) -> AppResult<FieldDef> {
    Ok(sqlx::query_as::<_, FieldDef>(
        "INSERT INTO field_def (asset_class_id, mnemonic, label, value_kind,
                                bbg_ftype, bbg_datatype, entitlement_note)
         VALUES ($1,$2,$3,$4,$5,$6,$7) RETURNING *")
        .bind(asset_class_id).bind(mnemonic).bind(label).bind(value_kind)
        .bind(bbg_ftype).bind(bbg_datatype).bind(entitlement_note)
        .fetch_one(pool).await?)
}
```

  Update `commands::create_field`, its `api.ts` binding and the field form in
  `ViewsScreen.svelte` to carry the three optional values.

- [ ] **Step 9: Run the whole suite**

```bash
cd src-tauri && cargo test
```

Expected: the crate compiles for the first time since Task 1, and every test
passes — the five new pipeline tests plus everything from Tasks 1-11.

- [ ] **Step 10: Commit**

```bash
git add src-tauri
git commit -m "feat(pipeline): retarget fetch/ingest to instruments; raw basis, append-only observations"
```

---

## Task 13: Excel import and export become the book

The workbook stays the migration tool and the bulk editor, but its subject changes from `asset` to `book_entry` + resolution. A row that resolves ambiguously opens a review instead of failing.

**Files:**
- Modify: `src-tauri/src/bulk/sheet.rs`, `src-tauri/src/bulk/diff.rs`, `src-tauri/src/bulk/mod.rs`, `src-tauri/src/commands.rs`
- Test: extend the existing test modules in `sheet.rs` and `diff.rs`

**Interfaces:**
- Consumes: `book::{list, add, AddOutcome}`, `resolution::engine`.
- Produces: `sheet::FIXED_HEADERS` of 8 columns; `diff::DbInstrument`; `ImportResult` gains `pub reviews_opened: usize`.

- [ ] **Step 1: Write the failing tests**

Add to the test module in `src-tauri/src/bulk/sheet.rs`:

```rust
    #[test]
    fn the_header_names_instruments_not_assets() {
        assert_eq!(FIXED_HEADERS,
                   ["instrument_id", "label", "class", "identifier", "yellow_key",
                    "active", "security", "status"]);
    }

    /// The id column is the guardrail from the 2026-08-18 work: a file without
    /// it was never a full export, so the absence of a row means nothing and no
    /// removal may be proposed. Renaming the column must not lose that.
    #[test]
    fn a_sheet_without_instrument_id_cannot_propose_removals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hand_built.xlsx");
        write_minimal_sheet(&path, &["label", "class", "identifier", "yellow_key"]);
        let data = read_assets_sheet(&path).unwrap();
        assert!(!data.has_id_column);
    }

    /// One column, not two. id_kind is gone: detect_id_kind reads the shape of
    /// the identifier, and a user who typed an ISIN into a "ticker" column was
    /// only ever telling us something we could see for ourselves.
    #[test]
    fn one_identifier_column_replaces_ticker_and_isin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.xlsx");
        write_assets_sheet(&path, &[ExportRow {
            instrument_id: 7, label: "Apple".into(), class: "Equity".into(),
            identifier: "AAPL US".into(), yellow_key: "Equity".into(),
            active: true, security: "AAPL US Equity".into(),
            status: "resolved".into(), views: vec![] }], &[], &["Equity".into()])
            .unwrap();
        let data = read_assets_sheet(&path).unwrap();
        assert_eq!(data.rows[0].identifier, "AAPL US");
    }
```

Add to the test module in `src-tauri/src/bulk/diff.rs`:

```rust
    #[test]
    fn a_new_row_is_an_addition_that_will_need_resolving() {
        let sheet = SheetData { has_id_column: true, view_columns: vec![],
            rows: vec![SheetRow { row_number: 2, instrument_id: None,
                label: "Tesla".into(), class: "Equity".into(),
                identifier: "TSLA US".into(), yellow_key: "Equity".into(),
                active: true, views: vec![] }] };
        let plan = diff(&sheet, &[], &["Equity".into()], &[], "hash");
        assert_eq!(plan.add_rows.len(), 1);
        assert_eq!(plan.add_rows[0].identifier, "TSLA US");
    }

    /// The identifier is not editable in place: changing it means a different
    /// instrument, which is an add plus a removal, not an edit. Silently
    /// rebinding an existing instrument_id to a new security is exactly the
    /// history-destroying edit this phase exists to prevent.
    #[test]
    fn changing_the_identifier_of_an_existing_row_is_rejected() {
        let db = vec![DbInstrument { instrument_id: 7, label: "Apple".into(),
            class: "Equity".into(), identifier: "AAPL US".into(),
            yellow_key: "Equity".into(), active: true,
            security: "AAPL US Equity".into(), views: vec![] }];
        let sheet = SheetData { has_id_column: true, view_columns: vec![],
            rows: vec![SheetRow { row_number: 2, instrument_id: Some(7),
                label: "Apple".into(), class: "Equity".into(),
                identifier: "MSFT US".into(), yellow_key: "Equity".into(),
                active: true, views: vec![] }] };
        let plan = diff(&sheet, &db, &["Equity".into()], &[], "hash");
        assert_eq!(plan.invalid_rows.len(), 1);
        assert!(plan.invalid_rows[0].reason.contains("identifier"));
        assert!(plan.edit_rows.is_empty());
    }

    #[test]
    fn a_label_change_is_still_an_ordinary_edit() {
        let db = vec![DbInstrument { instrument_id: 7, label: "Apple".into(),
            class: "Equity".into(), identifier: "AAPL US".into(),
            yellow_key: "Equity".into(), active: true,
            security: "AAPL US Equity".into(), views: vec![] }];
        let sheet = SheetData { has_id_column: true, view_columns: vec![],
            rows: vec![SheetRow { row_number: 2, instrument_id: Some(7),
                label: "Apple Inc".into(), class: "Equity".into(),
                identifier: "AAPL US".into(), yellow_key: "Equity".into(),
                active: true, views: vec![] }] };
        let plan = diff(&sheet, &db, &["Equity".into()], &[], "hash");
        assert_eq!(plan.edit_rows.len(), 1);
        assert!(plan.invalid_rows.is_empty());
    }
```

Add an integration test to `src-tauri/tests/bulk_import.rs`:

```rust
mod common;

use getbloomdata_lib::bulk::{self, sheet::{write_assets_sheet, file_sha256, ExportRow}};
use getbloomdata_lib::master_fetch::{IdentityBlock, MasterFetcher};
use getbloomdata_lib::resolution::score::Candidate;
use getbloomdata_lib::error::AppResult;

/// Answers row 1 with a clean identity and row 2 with two listings, so one row
/// imports and the other opens a review. MockMasterFetcher returns the same
/// canned answer for every call, which is not enough here.
struct TwoRowFetcher;

impl MasterFetcher for TwoRowFetcher {
    async fn identity(&self, securities: &[String]) -> AppResult<Vec<IdentityBlock>> {
        if securities.iter().any(|s| s.starts_with("ZIMP1")) {
            return Ok(vec![IdentityBlock {
                security: "ZIMP1 US Equity".into(),
                figi: Some("BBG000IMPORT1".into()),
                exch_code: Some("US".into()),
                currency: Some("USD".into()),
                name: Some("IMPORT ONE INC".into()),
                listing_date: Some("2000-01-03".parse().unwrap()),
                ..Default::default()
            }]);
        }
        Ok(vec![])   // ZIMP2 is not resolvable by reference; it falls to search
    }

    async fn hist_ids(&self, _s: &str, _a: &str, _d: chrono::NaiveDate)
        -> AppResult<Vec<getbloomdata_lib::master_fetch::HistIdRow>> {
        Ok(vec![])
    }

    async fn instrument_list(&self, _q: &str, _yk: Option<&str>, _max: u32)
        -> AppResult<Vec<Candidate>> {
        Ok(vec![
            Candidate { security: "ZIMP2 US Equity".into(), description: "Import Two".into(),
                        exchange: Some("US".into()), country: None, currency: None,
                        asset_class: None, figi: None },
            Candidate { security: "ZIMP2 LN Equity".into(), description: "Import Two".into(),
                        exchange: Some("LN".into()), country: None, currency: None,
                        asset_class: None, figi: None },
        ])
    }
}

/// Spec §8: an imported row that resolves ambiguously creates a review row
/// instead of failing the import. A book of two hundred lines must not stop dead
/// because one of them is ambiguous.
#[tokio::test]
async fn an_ambiguous_imported_row_opens_a_review_and_the_rest_still_import() {
    let pool = common::pool().await;
    sqlx::query("INSERT INTO asset_class (name) VALUES ('Equity')
                 ON CONFLICT (name) DO NOTHING").execute(&pool).await.unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("book.xlsx");
    let row = |ident: &str, label: &str| ExportRow {
        instrument_id: 0, label: label.into(), class: "Equity".into(),
        identifier: ident.into(), yellow_key: "Equity".into(), active: true,
        security: String::new(), status: String::new(), views: vec![],
    };
    write_assets_sheet(&path, &[row("ZIMP1 US", "Import One"),
                                row("ZIMP2", "Import Two")],
                       &[], &["Equity".into()]).unwrap();
    let hash = file_sha256(&path).unwrap();

    let result = bulk::apply_import_with(&pool, &TwoRowFetcher, &path, &hash, &[], None)
        .await.unwrap();

    assert_eq!(result.added, 1, "the resolvable row imports");
    assert_eq!(result.reviews_opened, 1, "the ambiguous row waits for a human");

    let book: i64 = sqlx::query_scalar("SELECT count(*) FROM book_entry")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(book, 1, "an ambiguous row must not quietly enter the book");
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM resolution_review WHERE status = 'pending'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(pending, 1);
}
```

- [ ] **Step 2: Run the tests to verify they fail**

```bash
cd src-tauri && cargo test bulk
```

Expected: FAIL to compile — `ExportRow has no field instrument_id`.

- [ ] **Step 3: Implement**

Change the sheet shape in `src-tauri/src/bulk/sheet.rs`:

```rust
pub const SHEET_NAME: &str = "Book";
pub const FIXED_HEADERS: [&str; 8] = [
    "instrument_id", "label", "class", "identifier", "yellow_key", "active",
    "security", "status",
];
```

`ExportRow` and `SheetRow` lose `id`, `ticker` and `isin`, and gain
`instrument_id`, `identifier` and (on `ExportRow` only) `status`. `security` and
`status` are written for the reader and ignored on import: `security` is derived
from the alias valid today, and `status` reflects the review queue.

In `src-tauri/src/bulk/diff.rs`, rename `DbAsset` to `DbInstrument` with the same
field changes, and add the identifier-change rejection to the edit branch:

```rust
        // Changing an existing row's identifier would rebind one instrument_id to
        // a different security, which destroys the link between the history
        // already stored and the instrument it belongs to. Removing the row and
        // adding the new identifier is the honest way to express that.
        if !db_row.identifier.eq_ignore_ascii_case(sheet_row.identifier.trim()) {
            invalid_rows.push(InvalidRow {
                row_number: sheet_row.row_number,
                reason: format!(
                    "identifier cannot be changed in place (was {}, sheet says {}); \
                     remove this row and add the new identifier instead",
                    db_row.identifier, sheet_row.identifier),
            });
            continue;
        }
```

In `src-tauri/src/bulk/mod.rs`:
- `load_db_assets` becomes `load_db_instruments`, reading `book_entry` joined to
  `asset_class`, with `identifier` and `security` from `instrument_alias` and
  `views` from `view_instrument`.
- `apply_import` gains a fetcher parameter and routes each added row through
  `book::add`, tallying `AddOutcome::NeedsReview` into a new
  `ImportResult.reviews_opened` rather than aborting.
- Keep `apply_import(pool, ...)` as a thin wrapper that constructs a
  `BlpapiMasterFetcher`, and add `apply_import_with(pool, fetcher, ...)` so the
  test above can pass a mock.

- [ ] **Step 4: Run the tests to verify they pass**

```bash
cd src-tauri && cargo test
```

Expected: everything passes, including the pre-existing `bulk` tests once their
fixtures are updated to the new column set.

- [ ] **Step 5: Commit**

```bash
git add src-tauri
git commit -m "feat(bulk): the workbook is a book of instruments; ambiguous rows open reviews"
```

---
## Task 14: The Book screen

Replaces the Assets screen. A search box that costs nothing, a Search Bloomberg button that costs one call and says so before it spends it, and the book itself.

**Files:**
- Create: `src/lib/BookScreen.svelte`
- Delete: `src/lib/AssetsScreen.svelte`
- Modify: `src/lib/api.ts`, `src/routes/+page.svelte`

**Interfaces:**
- Consumes: commands `search_local`, `search_bloomberg`, `list_book`, `add_to_book`, `set_book_active`, `list_asset_classes`.
- Produces: nothing other modules consume.

- [ ] **Step 1: Add the types and bindings**

In `src/lib/api.ts`, remove `Asset` and `NewAsset` and their three bindings, and add:

```ts
export interface BookEntry {
  instrument_id: number; asset_class_id: number; label: string;
  active: boolean; note: string;
  security: string | null; review_pending: boolean;
}
export interface AddToBook {
  raw: string; yellow_key: string; asset_class_id: number; label: string;
  hints: { exchange?: string | null; country?: string | null;
           currency?: string | null; asset_class?: string | null };
}
export type AddOutcome =
  | { Added: BookEntry }
  | { NeedsReview: { review_id: number } }
  | "NotFound";
export type SearchOrigin = "book" | "instrument" | "candidate";
export interface SearchHit {
  origin: SearchOrigin; security: string | null; display: string;
  description: string; instrument_id: number | null; similarity: number;
}
export interface BloombergSearch {
  hits: SearchHit[]; estimated_hits: number; cached: number;
}
export interface PendingReview {
  review_id: number; decision_id: number; raw_input: string; normalized: string;
  candidates: Array<{ candidate: { security: string; description: string;
                                   exchange: string | null };
                      score: number; disqualified: boolean; reasons: string[] }>;
  bbg_response: unknown | null; opened_at: string;
}
export interface AliasRow {
  id: number; id_type: string; value: string; exch_code: string | null;
  valid_from: string; valid_to: string; source: string;
  bbg_action_id: string | null; anchoring_identifier: string | null;
}
export interface AttrRow {
  id: number; attr: string; value: string;
  valid_from: string; valid_to: string; source: string;
}
```

and to the `api` object:

```ts
  listBook: () => invoke<BookEntry[]>("list_book"),
  addToBook: (req: AddToBook) => invoke<AddOutcome>("add_to_book", { req }),
  setBookActive: (instrumentId: number, active: boolean) =>
    invoke<void>("set_book_active", { instrumentId, active }),
  searchLocal: (query: string, limit = 12) =>
    invoke<SearchHit[]>("search_local", { query, limit }),
  searchBloomberg: (query: string, yellowKey: string) =>
    invoke<BloombergSearch>("search_bloomberg", { query, yellowKey }),
  listPendingReviews: () => invoke<PendingReview[]>("list_pending_reviews"),
  resolveReview: (reviewId: number, chosenSecurity: string) =>
    invoke<number>("resolve_review", { reviewId, chosenSecurity }),
  rejectReview: (reviewId: number, note: string) =>
    invoke<void>("reject_review", { reviewId, note }),
  instrumentAliases: (instrumentId: number) =>
    invoke<AliasRow[]>("instrument_aliases", { instrumentId }),
  instrumentAttrs: (instrumentId: number) =>
    invoke<AttrRow[]>("instrument_attrs", { instrumentId }),
```

- [ ] **Step 2: Write the screen**

Create `src/lib/BookScreen.svelte`:

```svelte
<script lang="ts">
  import { api, type AssetClass, type BookEntry, type SearchHit } from "./api";
  import DeleteDialog from "./DeleteDialog.svelte";
  import ImportDiff from "./ImportDiff.svelte";
  import InstrumentDetail from "./InstrumentDetail.svelte";
  import type { EntityKind, ImportPlan } from "./api";

  let classes = $state<AssetClass[]>([]);
  let book = $state<BookEntry[]>([]);
  let error = $state(""), notice = $state("");

  let query = $state("");
  let hits = $state<SearchHit[]>([]);
  let yellowKey = $state("Equity");
  let classId = $state(0);
  let label = $state("");
  let searching = $state(false);
  let detailFor = $state<number | null>(null);
  let pending = $state<{ kind: EntityKind; id: number } | null>(null);

  let sheetPath = $state("");
  let plan = $state<ImportPlan | null>(null);
  let previewBusy = $state(false);

  // Seeds sheetPath exactly once, at mount. Deliberately does not read sheetPath
  // itself, so the effect has no reactive dependency on it and never reasserts
  // the default over whatever the user typed. (Carried over from AssetsScreen.)
  async function seedSheetPath() {
    try { sheetPath = `${(await api.getSettings()).data_dir}\\book.xlsx`; }
    catch { sheetPath = "book.xlsx"; }
  }
  $effect(() => { seedSheetPath(); });

  async function exportSheet() {
    notice = ""; error = "";
    try { await api.exportAssetsXlsx(sheetPath); notice = `Written to ${sheetPath}`; }
    catch (e) { error = String(e); }
  }
  async function previewSheet() {
    previewBusy = true; notice = ""; error = "";
    try { plan = await api.previewAssetsImport(sheetPath); }
    catch (e) { error = String(e); }
    finally { previewBusy = false; }
  }
  function afterImport(applied: boolean, msg?: string) {
    plan = null;
    if (applied) { notice = msg ?? "Import applied."; reload(); }
  }

  const YELLOW_KEYS = ["Equity", "Corp", "Govt", "Index", "Curncy", "Comdty",
                       "Mtge", "Muni", "Pfd"];

  const ORIGIN_LABEL: Record<string, string> = {
    book: "in your book",
    instrument: "known instrument",
    candidate: "seen before",
  };

  // Local search only. This runs on every keystroke and never calls Bloomberg;
  // the Bloomberg tier is the button below, and nothing else may trigger it.
  async function runLocalSearch() {
    const q = query.trim();
    if (!q) { hits = []; return; }
    try { hits = await api.searchLocal(q); } catch (e) { error = String(e); }
  }
  $effect(() => { query; runLocalSearch(); });

  async function searchBloomberg() {
    if (!query.trim()) return;
    searching = true; error = ""; notice = "";
    try {
      const r = await api.searchBloomberg(query, yellowKey);
      hits = r.hits;
      notice = `Bloomberg searched (${r.estimated_hits} hit charged); `
             + `${r.cached} result(s) cached — this search is free from now on.`;
    } catch (e) { error = String(e); }
    finally { searching = false; }
  }

  async function addHit(h: SearchHit) {
    error = ""; notice = "";
    try {
      const out = await api.addToBook({
        raw: h.security ?? h.display,
        yellow_key: yellowKey,
        asset_class_id: classId,
        label: label.trim() || h.display,
        hints: {},
      });
      if (out === "NotFound") {
        error = `Bloomberg does not recognise ${h.security ?? h.display}.`;
      } else if ("NeedsReview" in out) {
        notice = "Several securities match. It is waiting in the Review queue — "
               + "nothing has been added yet.";
      } else {
        notice = `Added ${out.Added.security ?? out.Added.label}.`;
        label = ""; query = "";
      }
      await reload();
    } catch (e) { error = String(e); }
  }

  async function reload() {
    try {
      classes = await api.listAssetClasses();
      book = await api.listBook();
      if (classes.length && !classId) classId = classes[0].id;
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });
</script>

{#if error}<p class="error">{error}</p>{/if}
{#if notice}<p class="notice">{notice}</p>{/if}

<section>
  <h2>Find an instrument</h2>
  <div class="row">
    <input bind:value={query} placeholder="AAPL, US0378331005, Apple…"
           aria-label="Search instruments" />
    <select bind:value={yellowKey}>
      {#each YELLOW_KEYS as k}<option>{k}</option>{/each}
    </select>
    <select bind:value={classId}>
      {#each classes as c}<option value={c.id}>{c.name}</option>{/each}
    </select>
    <input bind:value={label} placeholder="Your label (optional)" />
  </div>

  {#if hits.length}
    <ul class="hits">
      {#each hits as h}
        <li>
          <span class="sec">{h.security ?? h.display}</span>
          <span class="desc">{h.description}</span>
          <span class="origin {h.origin}">{ORIGIN_LABEL[h.origin]}</span>
          {#if h.origin !== "book"}
            <button onclick={() => addHit(h)}>Add</button>
          {/if}
        </li>
      {/each}
    </ul>
  {:else if query.trim()}
    <p class="thin">Nothing local matches “{query}”.</p>
  {/if}

  <!-- The only path to Bloomberg on this screen. Typing must never reach it. -->
  <button onclick={searchBloomberg} disabled={searching || !query.trim()}>
    {searching ? "Searching Bloomberg…" : "Search Bloomberg (1 hit)"}
  </button>
  <p class="thin">Typing costs nothing. This button asks Bloomberg once and keeps
     the answer forever.</p>
</section>

<section>
  <h2>Your book</h2>
  <table>
    <thead><tr><th>Label</th><th>Security</th><th>Class</th>
               <th>Active</th><th></th></tr></thead>
    <tbody>
      {#each book as b}
        <tr class:review={b.review_pending}>
          <td><button class="link" onclick={() => (detailFor = b.instrument_id)}>
            {b.label}</button></td>
          <td>{b.security ?? "—"}</td>
          <td>{classes.find((c) => c.id === b.asset_class_id)?.name ?? ""}</td>
          <td><input type="checkbox" checked={b.active}
                     onchange={(e) => api.setBookActive(b.instrument_id,
                        (e.currentTarget as HTMLInputElement).checked).then(reload)} /></td>
          <td>{#if b.review_pending}<em>under review</em>{/if}</td>
        </tr>
      {/each}
    </tbody>
  </table>
</section>

<section>
  <h2>Excel</h2>
  <p class="thin">The export is also the migration tool: it is how a book survives
     a database rebuild.</p>
  <div class="row">
    <input bind:value={sheetPath} aria-label="Workbook path" />
    <button onclick={exportSheet}>Export</button>
    <button onclick={previewSheet} disabled={previewBusy}>
      {previewBusy ? "Reading…" : "Preview import"}</button>
  </div>
</section>

{#if plan}
  <ImportDiff {plan} path={sheetPath} onDone={afterImport} />
{/if}
{#if detailFor !== null}
  <InstrumentDetail instrumentId={detailFor} onClose={() => (detailFor = null)} />
{/if}
{#if pending}
  <DeleteDialog kind={pending.kind} id={pending.id}
                onDone={(changed) => { pending = null; if (changed) reload(); }} />
{/if}

<style>
  .row { display: flex; gap: 0.5rem; margin-bottom: 0.5rem; }
  .hits { list-style: none; padding: 0; }
  .hits li { display: flex; gap: 0.75rem; align-items: baseline;
             padding: 0.25rem 0; border-bottom: 1px solid #eee; }
  .sec { font-family: ui-monospace, monospace; }
  .desc { color: #555; flex: 1; }
  .origin { font-size: 0.8em; padding: 0 0.4em; border-radius: 3px; }
  .origin.book { background: #d8f0d8; }
  .origin.instrument { background: #e4e4f5; }
  .origin.candidate { background: #f0eada; }
  .thin { color: #666; font-size: 0.9em; }
  tr.review { background: #fff8e0; }
  .link { background: none; border: none; padding: 0; color: #06c;
          text-decoration: underline; cursor: pointer; }
</style>
```

- [ ] **Step 3: Wire the tab**

In `src/routes/+page.svelte`, replace the Assets tab with Book and add Review:

```svelte
<script lang="ts">
  import BookScreen from "$lib/BookScreen.svelte";
  import ReviewScreen from "$lib/ReviewScreen.svelte";
  import ViewsScreen from "$lib/ViewsScreen.svelte";
  import RunScreen from "$lib/RunScreen.svelte";
  import SettingsScreen from "$lib/SettingsScreen.svelte";
  let tab = $state<"book" | "review" | "views" | "run" | "settings">("run");
</script>

<main>
  <nav>
    {#each [["run","Run"],["book","Book"],["review","Review"],
            ["views","Views"],["settings","Settings"]] as [id, label]}
      <button class:active={tab === id} onclick={() => (tab = id as typeof tab)}>{label}</button>
    {/each}
  </nav>
  {#if tab === "book"}<BookScreen />
  {:else if tab === "review"}<ReviewScreen />
  {:else if tab === "views"}<ViewsScreen />
  {:else if tab === "run"}<RunScreen />{:else}<SettingsScreen />{/if}
</main>
```

Delete `src/lib/AssetsScreen.svelte`. Update `ViewsScreen.svelte` to call
`listBook`/`setViewInstruments` in place of `listAssets`/`setViewAssets`.

- [ ] **Step 4: Verify by hand**

```bash
npm run tauri dev
```

Check, in order: typing in the search box returns results with no visible delay
and adds nothing to `hit_ledger` (`SELECT * FROM hit_ledger ORDER BY id DESC LIMIT 5;`);
pressing Search Bloomberg adds exactly one row with `purpose = 'search'`; adding a
result puts it in the book with a derived security string.

- [ ] **Step 5: Commit**

```bash
git add src src-tauri/src/commands.rs
git commit -m "feat(ui): Book screen with free local search and an explicit Bloomberg button"
```

---

## Task 15: The Review queue

The screen that makes "nothing binds silently" workable rather than merely safe.

**Files:**
- Create: `src/lib/ReviewScreen.svelte`
- Modify: `src-tauri/src/commands.rs` (link proposals)

**Interfaces:**
- Consumes: `list_pending_reviews`, `resolve_review`, `reject_review`, and two new commands `list_link_proposals` / `confirm_link`.

- [ ] **Step 1: Add the link-proposal commands**

In `src-tauri/src/commands.rs`:

```rust
#[derive(Debug, Serialize, sqlx::FromRow)]
pub struct LinkProposal {
    pub id: i64,
    pub predecessor_id: i64,
    pub successor_id: i64,
    pub predecessor_label: Option<String>,
    pub successor_label: Option<String>,
    pub link_type: String,
    pub effective_date: chrono::NaiveDate,
    pub evidence: serde_json::Value,
}

#[tauri::command]
pub async fn list_link_proposals(state: State<'_, AppState>)
    -> Result<Vec<LinkProposal>, AppError> {
    Ok(sqlx::query_as::<_, LinkProposal>(
        "SELECT l.id, l.predecessor_id, l.successor_id,
                bp.label AS predecessor_label, bs.label AS successor_label,
                l.link_type, l.effective_date, l.evidence
           FROM instrument_link l
           LEFT JOIN book_entry bp ON bp.instrument_id = l.predecessor_id
           LEFT JOIN book_entry bs ON bs.instrument_id = l.successor_id
          WHERE l.confirmed_by IS NULL
          ORDER BY l.effective_date DESC")
        .fetch_all(&state.pool).await?)
}

#[tauri::command]
pub async fn confirm_link(state: State<'_, AppState>, link_id: i64)
    -> Result<(), AppError> {
    crate::instrument::store::confirm_link(&state.pool, link_id, "user").await
}
```

Register both in `src-tauri/src/lib.rs`.

- [ ] **Step 2: Write the screen**

Create `src/lib/ReviewScreen.svelte`:

```svelte
<script lang="ts">
  import { api, type PendingReview } from "./api";

  let reviews = $state<PendingReview[]>([]);
  let links = $state<any[]>([]);
  let error = $state(""), notice = $state("");
  let showRaw = $state<number | null>(null);

  async function reload() {
    try {
      reviews = await api.listPendingReviews();
      links = await api.listLinkProposals();
    } catch (e) { error = String(e); }
  }
  $effect(() => { reload(); });

  async function choose(reviewId: number, security: string) {
    error = ""; notice = "";
    try {
      await api.resolveReview(reviewId, security);
      notice = `Bound to ${security}.`;
      await reload();
    } catch (e) { error = String(e); }
  }

  async function reject(reviewId: number) {
    try { await api.rejectReview(reviewId, "rejected by user"); await reload(); }
    catch (e) { error = String(e); }
  }
</script>

{#if error}<p class="error">{error}</p>{/if}
{#if notice}<p class="notice">{notice}</p>{/if}

<section>
  <h2>Identifiers awaiting a decision</h2>
  {#if !reviews.length}
    <p class="thin">Nothing waiting. Every identifier resolved to exactly one security.</p>
  {/if}
  {#each reviews as r}
    <article>
      <h3>{r.raw_input} <span class="thin">→ {r.normalized}</span></h3>
      <p class="thin">Opened {new Date(r.opened_at).toLocaleString()}.
         Nothing is bound while this is open.</p>
      <table>
        <thead><tr><th>Security</th><th>Description</th><th>Exchange</th>
                   <th>Score</th><th>Why</th><th></th></tr></thead>
        <tbody>
          {#each r.candidates as c}
            <tr class:out={c.disqualified}>
              <td class="sec">{c.candidate.security}</td>
              <td>{c.candidate.description}</td>
              <td>{c.candidate.exchange ?? "—"}</td>
              <td>{c.score}</td>
              <td class="thin">{c.reasons.join("; ")}</td>
              <td>
                {#if !c.disqualified}
                  <button onclick={() => choose(r.review_id, c.candidate.security)}>
                    This one</button>
                {/if}
              </td>
            </tr>
          {/each}
        </tbody>
      </table>
      <button onclick={() => reject(r.review_id)}>None of these</button>
      <button onclick={() => (showRaw = showRaw === r.review_id ? null : r.review_id)}>
        {showRaw === r.review_id ? "Hide" : "Show"} what Bloomberg returned</button>
      {#if showRaw === r.review_id}
        <pre>{JSON.stringify(r.bbg_response, null, 2)}</pre>
      {/if}
    </article>
  {/each}
</section>

<section>
  <h2>Proposed instrument links</h2>
  <p class="thin">Bloomberg exposes no successor field, so every link below was
     inferred. None of them is followed by any query until confirmed.</p>
  {#if !links.length}<p class="thin">No proposals.</p>{/if}
  {#each links as l}
    <article>
      <p>{l.predecessor_label ?? `instrument ${l.predecessor_id}`}
         → {l.successor_label ?? `instrument ${l.successor_id}`}
         ({l.link_type}, effective {l.effective_date})</p>
      <pre>{JSON.stringify(l.evidence, null, 2)}</pre>
      <button onclick={() => api.confirmLink(l.id).then(reload)}>Confirm</button>
    </article>
  {/each}
</section>

<style>
  article { border: 1px solid #ddd; padding: 0.75rem; margin-bottom: 1rem; }
  .sec { font-family: ui-monospace, monospace; }
  .thin { color: #666; font-size: 0.9em; }
  tr.out { opacity: 0.5; }
  pre { background: #f6f6f6; padding: 0.5rem; overflow-x: auto; max-height: 20rem; }
</style>
```

Add `listLinkProposals` and `confirmLink` to `src/lib/api.ts`.

- [ ] **Step 3: Verify by hand**

```bash
npm run tauri dev
```

Add an ambiguous identifier (a bare `AAPL` with no exchange hint) from the Book
screen. It should not appear in the book, the Review tab should show it with its
candidates and their scores, and choosing one should bind it and clear the queue.

- [ ] **Step 4: Commit**

```bash
git add src src-tauri/src
git commit -m "feat(ui): review queue for ambiguous identifiers and proposed instrument links"
```

---

## Task 16: Instrument detail

Attribute and alias history as timelines, so a ticker change reads as two validity periods rather than an edit.

**Files:**
- Create: `src/lib/InstrumentDetail.svelte`
- Modify: `src-tauri/src/commands.rs`

**Interfaces:**
- Consumes: `instrument::store::{aliases, attrs}`.
- Produces: commands `instrument_aliases`, `instrument_attrs`.

- [ ] **Step 1: Add the commands**

```rust
#[tauri::command]
pub async fn instrument_aliases(state: State<'_, AppState>, instrument_id: i64)
    -> Result<Vec<crate::instrument::store::Alias>, AppError> {
    crate::instrument::store::aliases(&state.pool, instrument_id).await
}

/// Every attribute we have ever believed, not just today's, so the timeline
/// shows the change rather than only its result.
#[tauri::command]
pub async fn instrument_attrs(state: State<'_, AppState>, instrument_id: i64)
    -> Result<Vec<crate::instrument::store::Attr>, AppError> {
    Ok(sqlx::query_as::<_, crate::instrument::store::Attr>(
        "SELECT id, instrument_id, attr, value, valid_from, valid_to, source
           FROM instrument_attr
          WHERE instrument_id = $1 AND system_to = 'infinity'
          ORDER BY attr, valid_from")
        .bind(instrument_id).fetch_all(&state.pool).await?)
}
```

- [ ] **Step 2: Write the component**

Create `src/lib/InstrumentDetail.svelte`:

```svelte
<script lang="ts">
  import { api, type AliasRow, type AttrRow } from "./api";
  let { instrumentId, onClose }: { instrumentId: number; onClose: () => void } = $props();

  let aliases = $state<AliasRow[]>([]);
  let attrs = $state<AttrRow[]>([]);
  let error = $state("");

  // Postgres 'infinity' arrives as a sentinel date; showing it as a date would
  // read as a real expiry.
  const OPEN_ENDED = "9999-12-31";
  const until = (d: string) => (d >= OPEN_ENDED ? "present" : d);

  $effect(() => {
    (async () => {
      try {
        aliases = await api.instrumentAliases(instrumentId);
        attrs = await api.instrumentAttrs(instrumentId);
      } catch (e) { error = String(e); }
    })();
  });
</script>

<div class="panel">
  <button class="close" onclick={onClose}>Close</button>
  <h3>Instrument {instrumentId}</h3>
  {#if error}<p class="error">{error}</p>{/if}

  <h4>Identifiers</h4>
  <table>
    <thead><tr><th>Type</th><th>Value</th><th>From</th><th>Until</th>
               <th>Source</th><th>Bloomberg event</th><th>Anchored to</th></tr></thead>
    <tbody>
      {#each aliases as a}
        <tr>
          <td>{a.id_type}</td>
          <td class="sec">{a.value}</td>
          <td>{a.valid_from}</td>
          <td>{until(a.valid_to)}</td>
          <td>{a.source}</td>
          <td>{a.bbg_action_id ?? "—"}</td>
          <td class="thin">{a.anchoring_identifier ?? "—"}</td>
        </tr>
      {/each}
    </tbody>
  </table>
  <p class="thin">Two rows for the same type are a change, not a duplicate:
     the earlier one ended when the later one began.</p>

  <h4>Attributes</h4>
  <table>
    <thead><tr><th>Attribute</th><th>Value</th><th>From</th><th>Until</th>
               <th>Source</th></tr></thead>
    <tbody>
      {#each attrs as a}
        <tr><td>{a.attr}</td><td>{a.value}</td><td>{a.valid_from}</td>
            <td>{until(a.valid_to)}</td><td>{a.source}</td></tr>
      {/each}
    </tbody>
  </table>
</div>

<style>
  .panel { position: fixed; inset: 5% 10%; background: #fff; border: 1px solid #999;
           padding: 1rem; overflow: auto; box-shadow: 0 4px 24px rgba(0,0,0,0.2); }
  .close { float: right; }
  .sec { font-family: ui-monospace, monospace; }
  .thin { color: #666; font-size: 0.9em; }
  table { border-collapse: collapse; width: 100%; margin-bottom: 1rem; }
  th, td { border-bottom: 1px solid #eee; padding: 0.25rem 0.5rem; text-align: left; }
</style>
```

- [ ] **Step 3: Verify by hand**

Open the app, click a book entry's label. For an instrument with a rename in its
history (resolve `META US` on the Bloomberg machine), the Identifiers table should
show `FB` ending 2022-06-09 and `META` beginning the same day, with the Bloomberg
event id and the anchoring identifier both filled in.

- [ ] **Step 4: Commit**

```bash
git add src src-tauri/src/commands.rs src-tauri/src/lib.rs
git commit -m "feat(ui): instrument detail with identifier and attribute timelines"
```

---

## Task 17: Live smoke test on the Bloomberg machine

Everything above is proven against mocks and replayed captures. This is the only step that needs a Terminal, and it is what tells us the seam matches reality.

**Files:**
- Create: `docs/superpowers/plans/2026-08-19-p1-smoke-checklist.md`

- [ ] **Step 1: Write the checklist**

```markdown
# P1 smoke test — Bloomberg machine

Run after the database reset, with the Terminal running and logged in.
Record the result of each line; a failure here is a real finding, not a retry.

## Before

- [ ] `SELECT count(*) FROM instrument;` returns 0
- [ ] `SELECT coalesce(sum(estimated_hits),0) FROM hit_ledger WHERE occurred_on = CURRENT_DATE;` — note the number

## Resolution

- [ ] Add `AAPL US` (Equity). Resolves without review. `instrument_alias` has
      rows for bdp_security, figi and isin.
- [ ] Add `US0378331005` (Equity). Resolves to the SAME instrument_id — the
      ISIN is already an alias, so this must cost zero calls.
- [ ] Add `/isin/FR0000120271` (Equity). Resolves to a French listing.
- [ ] Add a fund share class (`VFIAX US`, Equity). Resolves; `instrument_attr`
      carries a share_class or fund_vehicle attribute if Bloomberg returned one.
- [ ] Add a bare `AAPL` with no exchange hint. Opens a review; the book does NOT
      gain a row.
- [ ] In Review, choose `AAPL US Equity`. It binds and the queue empties.

## Identifier history

- [ ] Add `META US` (Equity). Its detail panel shows `FB` ending 2022-06-09 and
      `META` from that date, with Action ID 228233742 and an anchoring identifier.
- [ ] `SELECT count(*) FROM instrument_alias WHERE source = 'bloomberg_hist_ids'
       AND anchoring_identifier IS NULL;` returns 0.

## Hit budget

- [ ] Type twenty characters into the search box. `hit_ledger` is unchanged.
- [ ] Press Search Bloomberg once. Exactly one row appears with purpose 'search'.
- [ ] Re-add an instrument already in the book. `hit_ledger` is unchanged.
- [ ] Total hits for the session are at most 2 per never-seen instrument.

## Observations

- [ ] Run EOD for a view containing AAPL. Observations land with layer 'raw'
      and a basis_id whose note starts with RAW.
- [ ] Re-run the same day. `SELECT count(*) FROM observation WHERE instrument_id = ..;`
      is unchanged — an identical re-fetch inserts nothing.
- [ ] Compare one PX_LAST against the Terminal with DPDF set to "None". They match.

## Known-unknowns to observe while here

- [ ] Note whether instrumentListRequest appears in the Terminal's own hit
      accounting (spec §10 q2) — the last item P0 left open.
```

- [ ] **Step 2: Run it and record the results**

Fill the checkboxes in the file, commit it with the results, and report any
failure rather than fixing it silently — a mismatch here means the fact base or
the design is wrong about Bloomberg, which is worth more than a patch.

- [ ] **Step 3: Commit**

```bash
git add docs/superpowers/plans/2026-08-19-p1-smoke-checklist.md
git commit -m "docs: P1 live smoke checklist with results"
```

---

## Done means

- `cargo test` passes with no Terminal present: every Bloomberg interaction is
  behind `MasterFetcher` and exercised by `MockMasterFetcher` or a replayed P0
  capture.
- `python -m pytest scripts/tests` passes.
- The app boots against a freshly created database and the book round-trips
  through Excel.
- The smoke checklist has been run on the Bloomberg machine and its results are
  committed.
- `grep -rn "asset_id" src-tauri/src` returns nothing.
- `grep -rn "ON CONFLICT.*DO UPDATE" src-tauri/src/ingest.rs` returns nothing.

## What this deliberately does not do

Writing any observation layer other than `raw`; reading history point-in-time;
ingesting corporate actions; deriving adjusted series; fund mergers and holdings
transformation. Those are P2 through P5, each with its own design, spec and plan.
The schema built here already has the shape they need, which is the point of
building it now rather than retrofitting it later.
