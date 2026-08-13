# Bloomberg EOD Data Pipeline — Design

**Date:** 2026-08-13
**Status:** Approved by user (sections 1–3 approved in brainstorming session)

## 1. Purpose

A Windows desktop tool that builds, day by day, a large local market-data
database from Bloomberg end-of-day data, while staying far below the
Bloomberg Terminal daily hit limit (~500,000 hits/day; Bloomberg's exact
definition of a hit is neither trivial nor public, so the tool treats its
own estimates as conservative approximations).

The user defines **views**: named watchlists of financial assets grouped by
asset class, each class with its own configurable set of Bloomberg fields.
One button press (or a daily scheduled run) executes the full round trip:

1. Generate an Excel workbook containing Bloomberg Add-in formulas.
2. Drive Excel through COM to open the workbook, let the Bloomberg Add-in
   resolve all formulas, save, and quit — fully unattended.
3. Read the calculated values back and validate them.
4. Ingest them idempotently into a local PostgreSQL + TimescaleDB database.

A later phase (out of scope here, but shaping the schema) adds PL/pgSQL
functions in the database to compute derived quantities such as variances
and covariances directly from the stored observations.

### Constraints

- Runs on the same Windows 11 machine as the Bloomberg Terminal, Excel, and
  the Bloomberg Excel Add-in (Desktop API licensing requires this).
- The user holds a standard Bloomberg Terminal license; the tool must never
  come near the daily limit and must warn long before it could.
- Data fields are not fixed: adding a field must require configuration
  only, never a code change or schema migration.
- Assets are identified by Bloomberg ticker **or** ISIN + yellow key,
  chosen per asset.

## 2. Architecture

### 2.1 Chosen approach

**Rust/Tauri core + bundled PowerShell script for Excel COM automation.**

Two alternatives were considered and rejected:

- *Late-bound IDispatch COM directly from Rust*: fragile, hard to debug,
  poor error surfaces; the COM interop is the riskiest part of the system
  and belongs in the thinnest, most debuggable layer possible.
- *Direct BLPAPI FFI*: cleanest long-term but heavier to build and it
  bypasses the Excel Add-in path the user knows and trusts today.

The PowerShell driver is ~50 lines, runnable and debuggable standalone from
a terminal, and spawned by Rust as a child process. The fetch stage sits
behind a Rust `DataFetcher` trait so the Excel/COM implementation can later
be swapped for a direct BLPAPI implementation without touching the rest of
the pipeline.

### 2.2 Components

```
┌─────────────────────────────────────────────────────┐
│ Tauri 2 desktop app (Windows 11)                    │
│                                                     │
│  Frontend: Svelte 5 + TypeScript                    │
│    - Assets screen   (create/edit assets & classes) │
│    - Views screen    (watchlists + field sets)      │
│    - Run dashboard   (launch, progress, history)    │
│    - Settings        (paths, schedule, thresholds)  │
│                                                     │
│  Rust core (crates: tauri, sqlx, tokio,             │
│             rust_xlsxwriter, calamine)              │
│    registry      assets & asset classes             │
│    fields        field catalog per asset class      │
│    views         view / view_asset / view_field     │
│    orchestrator  runs the 4-stage pipeline          │
│    excel_gen     builds .xlsx with BDP/BDH formulas │
│    refresh_driver spawns PowerShell COM script      │
│    excel_read    reads cached values via calamine   │
│    ingest        validation + idempotent upserts    │
│    scheduler     randomized daily trigger + gaps    │
│    budget        hit estimator + ledger + limits    │
└──────────────┬──────────────────────────────────────┘
               │ spawns                 │ sqlx
               ▼                        ▼
   powershell.exe refresh.ps1    PostgreSQL + TimescaleDB
   (Excel COM automation)        (local instance)
               │
               ▼
   Excel + Bloomberg Add-in ──► Bloomberg Terminal (Desktop API)
```

### 2.3 File layout on disk

- `pending/` — workbooks generated for the current run.
- `archive/YYYY/MM/` — workbooks moved here after successful ingest, named
  `run_<run_id>_<view>_<date>.xlsx`. Kept as the audit trail of exactly
  what Bloomberg returned.

### 2.4 `DataFetcher` trait

```rust
trait DataFetcher {
    async fn fetch_eod(&self, req: EodRequest) -> Result<Vec<Observation>>;
    async fn fetch_history(&self, req: HistoryRequest) -> Result<Vec<Observation>>;
}
```

The only implementation now is `ExcelComFetcher` (stages 1–3 of the
pipeline). A future `BlpapiFetcher` slots in behind the same trait.

## 3. Data model (PostgreSQL + TimescaleDB)

### 3.1 Reference tables

- `asset_class(id, name, description)`
- `asset(id, asset_class_id, label, id_kind ('ticker'|'isin'),
  ticker, isin, yellow_key, bdp_security TEXT NOT NULL, active BOOL)`
  - `bdp_security` is the resolved Bloomberg security string computed at
    save time — e.g. `AAPL US Equity`, `SX5E Index`, or
    `/isin/FR0000120271 Corp` — so the pipeline never re-derives it.
- `field_def(id, asset_class_id, mnemonic, label, value_kind
  ('numeric'|'text'|'date'), active BOOL)` — the configurable field
  catalog; adding a field is an INSERT, never a migration.

### 3.2 Views (watchlists)

- `view(id, name, description, active)`
- `view_asset(view_id, asset_id)`
- `view_field(view_id, field_id)` — per-view field selection; defaults to
  all active fields of the asset's class.

### 3.3 Observations (the core time series)

Long/EAV format — one row per (asset, field, date):

```sql
CREATE TABLE observation (
  asset_id   BIGINT      NOT NULL REFERENCES asset(id),
  field_id   BIGINT      NOT NULL REFERENCES field_def(id),
  obs_date   DATE        NOT NULL,
  value_num  DOUBLE PRECISION,
  value_text TEXT,
  run_id     BIGINT      NOT NULL REFERENCES run(id),
  ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (asset_id, field_id, obs_date)
);
SELECT create_hypertable('observation', 'obs_date');
```

- Exactly one of `value_num` / `value_text` is set, per
  `field_def.value_kind` (CHECK constraint).
- **Idempotency:** ingest uses
  `INSERT ... ON CONFLICT (asset_id, field_id, obs_date) DO UPDATE`, so
  retries, re-runs, and backfills never duplicate and always converge to
  the latest Bloomberg answer.
- The long format is precisely the tidy shape the future PL/pgSQL
  variance/covariance functions want to consume.

### 3.4 Operations tables

- `run(id, view_id, kind ('eod'|'backfill'), trigger ('manual'|'scheduled'),
  status ('generating'|'refreshing'|'reading'|'ingesting'|'ok'|'failed'|
  'partial'), started_at, finished_at, workbook_path, estimated_hits,
  error_summary)`
- `ingest_issue(id, run_id, asset_id, field_id, obs_date, severity,
  code, detail)` — one row per problem cell (see §6).
- `hit_ledger(id, run_id, estimated_hits, occurred_on)` — cumulative daily
  estimate powering the budget checks.
- `schedule(id, view_id, active, window_start TIME, window_end TIME,
  drawn_for DATE, drawn_at TIME, last_result)` — see §5.

## 4. The pipeline (one run = one workbook)

A run always produces **exactly one Excel workbook for the whole view**,
regardless of asset count. There is never one file per asset.

### Stage 1 — Generate (`excel_gen`, rust_xlsxwriter)

**EOD run:** one visible worksheet **per asset class** in the view (each
class has its own field set): row 1 = field mnemonics, column A = the
asset's `bdp_security`, each data cell =
`=BDP($A<row>, <mnemonic>)`.

**Backfill run:** one worksheet **per asset** (still in the single
workbook): a `BDH(security, fields, start, end, "Dates=S")` block spills a
variable-height dates × fields table, and per-asset sheets keep spill
ranges from colliding.

A hidden `META` sheet carries `run_id`, `view_id`, run kind, generation
timestamp, and a layout version number, so the reader stage can verify it
is reading the workbook it expects.

The workbook is written to `pending/`.

### Stage 2 — Refresh (`refresh_driver` → `refresh.ps1`)

Rust spawns `powershell.exe -NoProfile -File refresh.ps1 <workbook>
<timeout_s>`. The script:

1. Starts Excel via COM (`Visible = $false`, `DisplayAlerts = $false`).
2. Opens the workbook; waits for the Bloomberg Add-in to load.
3. Calls `Application.Run "RefreshAllStaticData"` (the add-in's documented
   refresh entry point).
4. Polls the used ranges until no cell still shows
   `#N/A Requesting Data...`, or the timeout elapses.
5. Saves, closes the workbook, quits Excel, and **kills any orphaned Excel
   process it started** in a `finally` block.
6. Exits with a machine-readable code: `0` ok, `2` timeout,
   `3` excel/COM error — plus a one-line JSON status on stdout.

The script is standalone-debuggable: run it by hand on any workbook.

### Stage 3 — Read & validate (`excel_read`, calamine)

Calamine reads the **cached calculated values** (not formulas) from the
saved workbook. Per cell:

- `#N/A N/A`, `#N/A Invalid Security`, `#N/A Field Not Applicable`, empty →
  recorded as an `ingest_issue`, not an observation.
- Type check against `field_def.value_kind`.
- META sheet checked against the expected `run_id` / layout version.

### Stage 4 — Ingest (`ingest`, sqlx)

Valid cells become `observation` upserts in one transaction per run.
On success the workbook moves from `pending/` to `archive/YYYY/MM/` and
the run is marked `ok` (or `partial` if issues were recorded).

## 5. Scheduling, gaps, and backfill

### 5.1 Randomized daily schedule

- The user configures a **time window** per schedule (default
  **09:00–18:00 local**), not a fixed time.
- Each day, the scheduler draws a uniformly random time (minute/second
  granularity) inside the window and persists it
  (`schedule.drawn_for = today`, `schedule.drawn_at = the draw`).
  Persisting the draw means an app restart neither re-rolls the time nor
  double-fires.
- The run fires at the drawn time; a new draw happens the next day.
- If the machine was off or asleep past the drawn time, the run fires at
  next app launch that day (catch-up). Fully missed days are handled by
  gap detection below.

*Note:* irregular timing makes the schedule look non-mechanical, but the
real license protection is the hit budget (§7) keeping usage far below the
daily limit. Bloomberg's terms govern automated use regardless of timing;
compliance remains the license holder's responsibility.

### 5.2 Gap detection — no holiday calendar

There is deliberately **no holiday-calendar table** to maintain:

- A *candidate gap* is any weekday with no observations for an
  (asset, field) that is active in a scheduled view.
- Backfill issues a `BDH` over the gap range. BDH returns **only trading
  days**; any weekday it does not return was a holiday, and the tool
  records nothing for it. The database thereby converges on Bloomberg's
  own view of the trading calendar with zero maintenance.

### 5.3 Backfill limits

- Backfill range is capped (default **30 days**) per run.
- Before a backfill run, the tool shows the estimated hit cost and the
  range, and requires explicit confirmation (manual runs) or splits into
  capped chunks across days (scheduled catch-up).

## 6. Error handling

| Failure | Behavior |
|---|---|
| Excel/COM error (exit 3) | Run `failed`; workbook stays in `pending/` for manual inspection; stderr captured into `run.error_summary`. |
| Refresh timeout (exit 2) | One automatic retry with doubled timeout; then `failed`. |
| Orphaned Excel process | `refresh.ps1` kills the Excel PID it started in its `finally` block; orchestrator also checks post-exit. |
| Invalid security / field per cell | `ingest_issue` row (`severity='warn'`), run continues; run ends `partial`. |
| Type mismatch in a cell | Same as above, `code='type_mismatch'`. |
| META mismatch (wrong/stale workbook) | Run `failed` before any ingest. |
| DB unavailable | Run `failed` at stage 4; workbook remains in `pending/`; re-running ingest later is safe (idempotent upserts). |
| Budget hard threshold exceeded | Run refuses to start until the user confirms in the UI. |

`ingest_issue` gives per-cell granularity: one bad ISIN never poisons a
200-asset run.

## 7. Hit budget

Bloomberg's hit accounting is not public, so the tool uses a conservative
estimator and treats it as a soft signal, not a guarantee:

- **BDP** ≈ 1 hit per security × field.
- **BDH** ≈ 1 hit per security × field × returned day.
- Every run's estimate is written to `hit_ledger`; the day's cumulative
  total is shown on the Run dashboard.
- **Soft warning** at a configurable daily threshold (default **100,000** —
  20% of the assumed limit): the UI warns but allows.
- **Hard confirm** above the soft threshold ×2: the run will not start
  without explicit user confirmation.
- Every run (manual or scheduled) shows/logs its estimated cost *before*
  touching Excel.

## 8. Testing strategy

- **excel_gen:** unit tests — generate a workbook, re-read it with
  calamine, assert formula strings and META contents. No Excel needed.
- **excel_read + ingest:** fixture workbooks (hand-saved with real
  Bloomberg output shapes, including `#N/A` variants) checked into the
  repo; assert produced observations and issues.
- **ingest idempotency:** property test — ingest the same fixture twice,
  assert row counts and values unchanged.
- **refresh.ps1:** manual/integration only (needs the real machine); a
  `--dry-run` flag opens/saves a formula-free workbook to exercise the COM
  path without Bloomberg.
- **scheduler:** unit tests over an injected clock — draw persistence,
  no-double-fire, catch-up, gap detection over synthetic weekday grids.
- **budget:** unit tests of the estimator against known view shapes.
- **End-to-end smoke test:** a 2-asset, 3-field view run on the real
  machine, verified by querying `observation`.

## 9. Out of scope (this phase)

- PL/pgSQL derived-data functions (variance, covariance, …) — next phase;
  the long-format `observation` table is designed for them.
- Direct BLPAPI fetcher — future `DataFetcher` implementation.
- Multi-machine or server deployment — the Desktop API license binds the
  tool to the Terminal machine.
- Intraday data — EOD only.
