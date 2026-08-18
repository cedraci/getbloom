# Bloomberg EOD Data Pipeline — BLPAPI Redesign (Amendment A2)

**Date:** 2026-08-18
**Status:** IMPLEMENTED 2026-08-18 — all 11 migration steps complete, verified end to end against the live Terminal (see §12, §13)
**Supersedes:** §2 (Architecture), §4 (Pipeline), §6 (Error handling),
§7 (Hit budget), §8 (Testing) of `2026-08-13-bloomberg-eod-pipeline-design.md`.
**Preserves unchanged:** §3 (Data model), §5 (Scheduling/gaps/backfill),
and the semantics of Amendment A1 (daily runs target the previous trading day).

---

## 1. Why this reverses the original decision

The 2026-08-13 design (§2.1) considered and rejected direct BLPAPI:

> *Direct BLPAPI FFI: cleanest long-term but heavier to build and it
> bypasses the Excel Add-in path the user knows and trusts today.*

Three things have changed since:

1. **The Excel path's central assumption is unverified and may not hold.**
   `refresh.ps1` creates Excel via COM with `Visible = $false`. An Excel
   instance created that way does not necessarily load XLL/XLA add-ins the
   way an interactively launched Excel does. If the Bloomberg add-in does
   not load there, every cell returns `#NAME?` and no amount of waiting
   helps. The whole pipeline rests on this one untested behaviour.

2. **The failure modes are structurally silent.** `refresh.ps1` line 48
   swallows a `RefreshAllStaticData` exception into `$detail`, and line 61
   then overwrites `$detail` with `''` and exits `0`. A broken add-in
   produces a *successful-looking* run that ingests nothing. This is less a
   one-line bug than a symptom: Excel communicates failure through cell
   strings (`#N/A Invalid Security`, `#N/A Requesting Data`) that must be
   sniffed and guessed at, so silence is the default outcome.

3. **Excel is why the end-to-end path cannot be tested.** The Excel layer
   is ~1,170 lines and is the sole reason the orchestrator's happy path
   needs a Bloomberg Terminal and an Excel installation to exercise at all.

BLPAPI replaces string-sniffing with **structured errors**, one-workbook-
per-run with **batched requests**, a 600-second poll loop with a
**seconds-scale event loop**, and an untestable path with a **mockable
trait**. The original reason to prefer Excel — that it is the path the user
already trusts — is exactly what Step 2 of the test plan puts in doubt.

---

## 2. Architecture

### 2.1 Chosen approach — Rust core + Python BLPAPI sidecar

**Keep the process-driver shape; change what sits at the end of it.**

The current design's best property is that its riskiest part is a small,
standalone-runnable script a human can execute from a terminal and watch.
That property is preserved exactly: `powershell.exe refresh.ps1` becomes
`python.exe blp_fetch.py`, and `refresh_driver.rs` (spawn, read exit code,
parse the last JSON line) survives almost verbatim as `blp_driver.rs`.

Bloomberg supports the Python BLPAPI package officially, so the riskiest
layer of the system moves onto a documented, first-party API instead of
resting on undocumented add-in macro names and localized error strings.

**Rejected alternatives:**

- *Rust FFI to the C SDK.* Bloomberg's C++ headers are inline wrappers over
  a genuine C ABI, so bindgen is viable — but it requires the SDK present
  at build time, MSVC link configuration in `build.rs`, and either
  unofficial crates (`blpapi_sys`) or hand-maintained bindings. Correct
  long-term, wrong first step.
- *.NET sidecar.* Officially supported, and .NET is usually already present
  on a Terminal machine — but it is more code and a second toolchain in the
  repo for no gain over Python.

**This is explicitly a seam, not a destination.** The sidecar sits behind
the `DataFetcher` trait. If the Python runtime becomes an operational
problem, a Rust FFI fetcher replaces it without touching the orchestrator,
the scheduler, the database, or the UI.

### 2.2 Components

```
+-----------------------------------------------------+
| Tauri 2 desktop app (Windows 11)                    |
|                                                     |
|  Frontend: Svelte 5 + TypeScript      (UNCHANGED)   |
|                                                     |
|  Rust core (tauri, sqlx, tokio)                     |
|    registry      assets & asset classes  UNCHANGED  |
|    fields        field catalog           UNCHANGED  |
|    views         view membership         UNCHANGED  |
|    scheduler     randomized trigger      UNCHANGED  |
|    ingest        idempotent upserts      UNCHANGED  |
|    budget        estimator + ledger      recalibrate|
|    orchestrator  3-stage pipeline        simplified |
|    fetch         request planning + types      NEW  |
|    blp_driver    spawns the Python sidecar     NEW  |
|                                                     |
|    excel_gen / excel_read / refresh_driver   DELETED|
+--------------+--------------------------------------+
               | spawns (JSON stdin/stdout)  | sqlx
               v                             v
      python blp_fetch.py            PostgreSQL + TimescaleDB
               |
               v  blpapi, localhost:8194
      Bloomberg Terminal (Desktop API)
```

### 2.3 What is deleted, kept, and changed

| | Component | Fate |
|---|---|---|
| **Deleted** | `excel_gen.rs` (373 lines) | workbook generation is gone |
| | `excel_read.rs` (631 lines) | no cached values to parse |
| | `refresh_driver.rs` (85 lines) | replaced by `blp_driver.rs` |
| | `scripts/refresh.ps1` (83 lines) | replaced by `scripts/blp_fetch.py` |
| | deps `rust_xlsxwriter`, `calamine` | no longer needed |
| | `LAYOUT_VERSION`, `META` sheet, `check_meta` | no workbook to identify |
| | `sanitize_sheet_name`, `bdh_sheet_name` | no sheets |
| | `excel_serial_to_date` | BLPAPI returns typed dates |
| | `AppError::Excel`, `AppError::Refresh` | replaced by `AppError::Blp` |
| | issue codes `requesting`, `empty`, `bad_date` | no such states exist |
| **Kept** | entire DB schema (§3) | one small migration, see §3 |
| | `registry`, `fields`, `views`, `ingest` | untouched |
| | `scheduler`, including A1 previous-weekday logic | untouched |
| | all Svelte screens and `api.ts` | untouched |
| | the audit-trail property | now JSON, see §4.4 |
| **Changed** | `DataFetcher` trait | cleaned, see §2.4 |
| | `orchestrator` | 4 stages become 3 |
| | `budget` constants | recalibrated, see §7 |
| | `PipelineConfig` | `script_path` → `python_path` + `script_path`; `refresh_timeout_s` → `request_timeout_s` |

Net: roughly **1,170 lines deleted**, roughly **400 added** (a ~180-line
Python sidecar, a ~90-line driver, ~130 lines of request planning).

### 2.4 The new `DataFetcher` trait

The current trait leaks Excel through every parameter — it takes a workbook
path and a `WbMeta`, consumes `GenAsset`/`GenField` (generation types), and
returns `excel_read::ReadOutcome`. Nothing but an Excel implementation can
satisfy it. Replaced by:

```rust
pub struct FetchRequest {
    pub run_id: i64,
    pub assets: Vec<FetchAsset>,      // asset_id, bdp_security, asset_class_id
    pub fields: Vec<FetchField>,      // field_id, asset_class_id, mnemonic, value_kind
    pub start: NaiveDate,
    pub end: NaiveDate,               // start == end for an EOD run
}

pub struct FetchOutcome {
    pub cells: Vec<ObsCell>,          // asset_id, field_id, obs_date, CellValue
    pub problems: Vec<CellProblem>,   // asset_id, field_id, obs_date, code, detail
}

pub trait DataFetcher {
    fn fetch(&self, req: &FetchRequest)
        -> impl Future<Output = AppResult<FetchOutcome>> + Send;
}
```

One method instead of two: an EOD run is a history request where
`start == end`, which is precisely what Amendment A1 already established.
`ObsCell` and `CellProblem` keep their current shapes and move from
`excel_read` into a new `fetch` module, so **`ingest.rs` needs no changes
at all**.

The payoff: `MockFetcher` becomes ~20 lines, and the orchestrator's entire
end-to-end path becomes unit-testable with neither Excel nor Bloomberg.

---

## 3. Data model

Unchanged, except one migration (`0003_blpapi.sql`):

- `run.workbook_path` → renamed `payload_path` (nullable). It now points at
  the archived JSON response rather than an `.xlsx`.
- `run.status` CHECK constraint: replace `refreshing` with `fetching`, and
  drop `generating` / `reading` (there is no generate stage, and reading is
  no longer a distinct phase). New set:
  `pending | fetching | ingesting | ok | failed | partial`.
- `ingest_issue.code` is free text today, so the new codes below need no
  migration.

Everything else — `asset`, `field_def`, `view*`, `observation` (including
the `(asset_id, field_id, obs_date)` primary key and the hypertable),
`hit_ledger`, `schedule` — is untouched. **`bdp_security` keeps its name
and its format**: `AAPL US Equity` and `/isin/FR0000120271 Corp` are
exactly the security strings BLPAPI expects, so
`registry::resolve_bdp_security` needs no change whatsoever.

---

## 4. The new pipeline (3 stages)

### Stage 1 — Plan (`fetch::plan_requests`)

Group the view's assets and fields into BLPAPI requests:

- **numeric / date fields** → `//blp/refdata` **`HistoricalDataRequest`**,
  with `startDate = endDate = obs_date` for a daily run, or the full range
  for a backfill. Securities and fields are **batched**: one request per
  asset class carrying up to `MAX_SECURITIES_PER_REQUEST` (default 100)
  securities × that class's numeric/date mnemonics.
- **text fields** → `//blp/refdata` **`ReferenceDataRequest`**, batched the
  same way, stamped with the run's `obs_date`.

This preserves Amendment A1 exactly — history for anything with a close,
live reference data for text fields under the previous-day date — but the
mechanism collapses from *one worksheet per asset* to *a handful of batched
requests*. A 200-asset view goes from 200 BDH sheets to 2 requests.

### Stage 2 — Fetch (`blp_driver` → `blp_fetch.py`)

Rust spawns `python.exe blp_fetch.py` and writes the request JSON to its
**stdin** — not argv, because a 200-security batch would blow past the
Windows command-line length limit. The sidecar:

1. Opens a `blpapi.Session` against `localhost:8194`.
2. Opens the `//blp/refdata` service.
3. Sends each request and drains events until `RESPONSE`, accumulating
   `PARTIAL_RESPONSE` events along the way.
4. Emits results as JSON on **stdout**, last line = a status object.
5. Exits with a machine-readable code.

**Wire protocol — request (stdin):**

```json
{
  "run_id": 42,
  "timeout_s": 120,
  "requests": [
    {"kind": "history",
     "securities": ["AAPL US Equity", "SX5E Index"],
     "fields": ["PX_LAST", "PX_VOLUME"],
     "start": "20260814", "end": "20260814"},
    {"kind": "reference",
     "securities": ["AAPL US Equity"],
     "fields": ["NAME"],
     "obs_date": "2026-08-14"}
  ]
}
```

**Wire protocol — response (stdout, last line):**

```json
{"status": "ok", "seconds": 3.4, "detail": "",
 "observations": [
   {"security": "AAPL US Equity", "field": "PX_LAST",
    "date": "2026-08-14", "num": 231.42},
   {"security": "AAPL US Equity", "field": "NAME",
    "date": "2026-08-14", "text": "APPLE INC"}],
 "problems": [
   {"security": "XXXX US Equity", "field": "PX_LAST",
    "date": "2026-08-14", "code": "invalid_security",
    "detail": "Unknown/Invalid Security"}]}
```

Results are keyed by **security string**, not `asset_id` — the sidecar
knows nothing about the database. `blp_driver` maps security → `asset_id`
and mnemonic → `field_id` on the way back, and raises `unknown_security`
for anything it did not ask for.

**Exit codes:** `0` ok (including partial results), `2` timeout,
`3` session/service error, `4` malformed input.

**Non-negotiable rule, learned directly from `refresh.ps1`:** a failure to
start the session, open the service, or send a request **must** exit
non-zero. A `status: "ok"` carrying zero observations *and* zero problems
is treated by the driver as a fault, not a success. Silence is never
success.

### Stage 3 — Ingest (`ingest`, sqlx)

**Completely unchanged.** One transaction per run, `ON CONFLICT
(asset_id, field_id, obs_date) DO UPDATE`, then `ingest_issue` rows at
`severity='warn'`. The run ends `ok`, or `partial` if issues were recorded.

### 4.4 The audit trail

Today the archived workbook is the record of exactly what Bloomberg
returned, and that property is worth keeping. The raw sidecar response is
written to `archive/YYYY/MM/run_<id>_<view>_<date>.json` and its path stored
in `run.payload_path` — same directory convention, same naming, same
purpose, but a far smaller and more greppable file. `pending/` disappears:
there is no intermediate artifact that must survive between stages.

### 4.5 Error mapping

BLPAPI reports failures as structured elements rather than cell strings, so
classification becomes exact instead of heuristic:

| BLPAPI condition | issue code | note |
|---|---|---|
| `securityData.securityError`, category `BAD_SEC` | `invalid_security` | was `#N/A Invalid Security` |
| `securityError`, category `NOT_ENTITLED` | `not_entitled` | **new** — Excel could not distinguish this |
| `fieldExceptions[].errorInfo`, `NOT_APPLICABLE_TO_REF_DATA` / `BAD_FLD` | `field_not_applicable` | was `#N/A Field Not Applicable` |
| `fieldData` array empty for the requested date | `no_data` | holiday case; identical to A1 semantics |
| field present but datatype ≠ `field_def.value_kind` | `type_mismatch` | now exact — BLPAPI declares types |
| security in the response that was never requested | `unknown_security` | driver-side check |
| ~~`#N/A Requesting Data`~~ | *removed* | no async cell state exists |
| ~~empty cell~~ | *removed* | absent data is explicit, never blank |

**The holiday behaviour of Amendment A1 survives untouched:** a
`HistoricalDataRequest` for a non-trading day returns an empty `fieldData`
array, which becomes one `no_data` issue per (asset, field) and a `partial`
run — the same outcome as an empty BDH spill today, with the date correctly
remaining a gap in `missing_weekdays`.

---

## 5. Scheduling, gaps, and backfill

**Unchanged**, including the randomized draw, its persistence, the weekday
guard, `previous_weekday`, gap detection, and the 30-day backfill cap.

One clarification: the 30-day cap was never a mechanical limit — a single
`HistoricalDataRequest` can return years of data. It is a **budget policy**,
and it stays exactly as it is under §7.

---

## 6. Error handling

| Failure | Behavior |
|---|---|
| Session startup failure (exit 3) | Run `failed`; sidecar detail into `run.error_summary`. Most common cause: Terminal not running or not logged in. |
| Service open failure (exit 3) | Run `failed`, with a distinct detail string — this is an entitlement signal, not a connectivity one. |
| Request timeout (exit 2) | One automatic retry with doubled timeout, then `failed`. Same policy as today. |
| Malformed sidecar output, or exit 0 with an empty payload | Run `failed`. Explicitly **not** treated as success. |
| Python or the `blpapi` module missing | Run `failed` at spawn with an actionable message; also surfaced in Settings as a startup check. |
| Per-security or per-field error | `ingest_issue` (`severity='warn'`), run continues, ends `partial`. One bad ISIN never poisons a 200-asset run. |
| DB unavailable | Run `failed` at stage 3; the archived JSON payload is already on disk, so re-ingesting later is safe and idempotent. |
| Budget hard threshold exceeded | Run refuses to start until confirmed in the UI. Unchanged. |

Three rows from the old table are simply gone: Excel/COM errors, orphaned
Excel processes, and META mismatch. There is no Excel process to orphan and
no workbook whose identity could be stale.

---

## 7. Hit budget

The mechanism stays (estimator → `hit_ledger` → soft warn → hard confirm).
The **constants must be recalibrated and must not be assumed to carry
over**: the current estimator models the *Excel add-in's* accounting (1 hit
per security × field, × returned day for BDH), which is not the same
accounting Bloomberg applies to Desktop API requests. Neither is public.

Interim policy: keep the existing formula and the 100,000 soft threshold —
conservative in the same direction — but treat the numbers as provisional
until observed against real DAPI behaviour, and label the figure in the UI
as an estimate. Batching does **not** reduce the estimate: 100 securities in
one request still costs 100 securities' worth of data.

---

## 8. Testing strategy

This is where the redesign pays for itself.

- **`fetch::plan_requests`** — unit tests: given a mixed view, assert the
  exact request batching, that numeric/date fields go to `history` and text
  fields to `reference`, and that batches respect
  `MAX_SECURITIES_PER_REQUEST`. No Bloomberg, no Excel.
- **`blp_driver` parsing** — unit tests over checked-in JSON fixtures of
  real BLPAPI responses, covering every error shape in §4.5. This directly
  replaces today's fixture-workbook tests and is easier to author: the
  fixtures are readable JSON rather than binary `.xlsx`.
- **Contract tests** — the same fixture files are consumed by both the Rust
  driver tests and the Python sidecar tests, so the two halves of the
  protocol cannot drift apart silently.
- **Sidecar `--replay <file>`** — parses a canned response file instead of
  opening a session, making all sidecar logic testable with no Terminal.
- **Sidecar `--probe`** — opens a session, opens the service, prints the
  result, exits. The direct equivalent of today's `probe-bloomberg-com.ps1`,
  and the first thing to run on the Bloomberg machine.
- **`MockFetcher`** — implements the clean trait in ~20 lines, making
  `run_eod` and `run_backfill` testable end to end. **The Excel dependency
  disappears from `eod_pipeline_dry_run_ends_partial`**, so all six
  integration tests reduce to a single prerequisite: Postgres.
- **Unchanged:** scheduler, budget, and ingest-idempotency tests.
- **End-to-end smoke test** on the real machine: still required, still a
  2-asset, 3-field view verified by querying `observation`.

---

## 9. Constraints that do NOT change

Worth stating plainly, because the redesign buys no freedom here:

- The tool still runs on the **same Windows machine as the Terminal**, with
  the Terminal **running and logged in**. Desktop API connects to
  `localhost:8194` and is licensed to that machine and that user.
- Still no headless service, and still no "run whether user is logged on or
  not" — the Terminal itself requires the interactive session. The
  app-must-be-running problem is unchanged and still needs autostart plus a
  tray icon.
- EOD only; no intraday.
- Adding a field remains an INSERT, never a migration.

What *does* improve operationally: no Excel means no orphan processes, no
15-second add-in load gamble, no 600-second poll, and runs measured in
seconds rather than minutes.

---

## 10. Migration plan

Ordered so the risky, externally-dependent part is proven **before** any
working code is deleted.

1. **Write `scripts/blp_fetch.py`** with `--probe` and `--replay`.
2. **Verify on the Bloomberg machine:** `python blp_fetch.py --probe`
   connects and opens `//blp/refdata`. *This is the go/no-go gate — the
   BLPAPI equivalent of Step 2 in the Excel test plan, and it either
   confirms or kills the approach in ten minutes.*
3. **Verify a real single-day fetch** by hand for one security, comparing
   the value against the Terminal.
4. Record real responses — including a bad ticker and an inapplicable
   field — as **JSON fixtures**; write the sidecar's `--replay` tests.
5. **Add `fetch.rs`** — types, `plan_requests`, unit tests. Nothing is
   deleted yet; the Excel path still works.
6. **Add `blp_driver.rs`** plus parsing tests against the same fixtures.
7. **Reshape the `DataFetcher` trait** (§2.4) and add `BlpapiFetcher`. Both
   fetchers exist briefly, behind the same trait.
8. **Migration `0003_blpapi.sql`** — run status CHECK, `payload_path`.
9. **Switch the orchestrator** to `BlpapiFetcher`; add `MockFetcher` and the
   end-to-end orchestrator tests that were impossible before.
10. **Delete** `excel_gen.rs`, `excel_read.rs`, `refresh_driver.rs`,
    `refresh.ps1`, and the two Excel crates — only now, with the
    replacement proven green.
11. **Update the smoke-test checklist** (which also still needs its A1
    contradiction on line 14 fixed).

Steps 1–4 need the Bloomberg machine. Steps 5–11 do not.

---

## 11. Open questions

1. **Licensing — verify first.** The Excel add-in and `blpapi` are both
   Desktop API, so this is the same license class, but confirm with your
   Bloomberg representative that programmatic DAPI use from a custom
   application is covered by your terms. This is the only question that
   could invalidate the redesign outright, and it costs one email.
2. **Is Python permitted and installable on the Terminal machine?** If IT
   policy forbids it, fall back to the .NET sidecar — same protocol, same
   design, more code — rather than back to Excel.
3. **`blpapi` package install.** Bloomberg distributes it from their own
   package index rather than PyPI, and it may require the C++ SDK runtime on
   `PATH`. To be confirmed at step 1.
4. **Real DAPI limits**, for the §7 recalibration — observable only once
   runs are actually happening.

---

## 12. Verification log (2026-08-18, on the Bloomberg machine)

Steps 1-4 of the migration plan are **done**. Recorded here because two
findings amend the design above.

### Gate passed

`python scripts/blp_fetch.py --probe` connected to `localhost:8194`, opened
`//blp/refdata`, and returned live data in **0.8 s**:

```
  [ok]   blpapi module imported (version 3.26.7.1)
  [ok]   session started and //blp/refdata opened
  [ok]   IBM US Equity NAME = INTL BUSINESS MACHINES CORP
  [ok]   IBM US Equity PX_LAST = 228.85
  GREEN LIGHT: Desktop API works from a custom app (0.8s).
```

The Desktop API works from a custom application on this machine. Open
question 2 (Python permitted) and 3 (`blpapi` installable) are closed:
`blpapi 3.26.7.1` installs from Bloomberg's index as a `py3-none-win_amd64`
wheel -- no ABI tag, so the Python 3.14 runtime here is fine.

A full EOD-shaped run (2 securities x 2 history fields + a reference field,
`obs_date = 2026-08-17`) returned 6 observations, 0 problems, in **1.5 s**.
For comparison, the Excel path budgets a 15-second add-in sleep before it
even starts polling.

### Finding A -- Bloomberg silently accepts impossible dates (design change)

`HistoricalDataRequest` with `startDate = endDate = 20261301` returned
**no error at all**: an empty `fieldData` and an exit-0 response. Without a
guard, the sidecar minted a `no_data` issue dated `2026-13-01` -- a
malformed request laundered into a plausible-looking holiday.

This is the same failure class as `refresh.ps1` exiting 0 on a thrown
macro, so it is fixed the same way: **the sidecar validates every request
before sending it** (`validate_payload`), rejects unparseable or reversed
date ranges with exit `4`, and `parse_capture` applies the same check on
replay so a malformed fixture cannot pass either. Add to the §6 table:

| Failure | Behavior |
|---|---|
| Malformed request (bad/reversed dates, no securities, no fields) | Rejected before sending; exit `4`; run `failed`. Never reaches Bloomberg. |

### Finding B -- "invalid security" and "no data" are not always distinguishable

The §4.5 mapping is correct: a genuinely unknown ticker
(`ZQZQZQ99 US Equity`) returns `securityError` / `BAD_SEC` /
`INVALID_SECURITY` on **both** request kinds.

But a security that *resolves* at Bloomberg yet has no data for the
requested day returns an empty `fieldData` with **no error**, which is
byte-identical to the holiday case. Such a security is therefore reported
as `no_data`, not `invalid_security`. This is correct behaviour rather than
a defect -- Bloomberg genuinely cannot distinguish them -- but it means
**`no_data` is not proof of a holiday**. Anything downstream that infers a
market calendar from `no_data` would be wrong. Nothing does today, and
nothing should.

Two shape details worth noting for anyone extending the parser:
`HistoricalDataResponse` carries `securityData` as a **single object**,
`ReferenceDataResponse` as an **array**; and on a security error the
reference response sets `"fieldData": null`, not `{}`.

### Test assets produced

- `scripts/blp_fetch.py` -- the sidecar (`--probe`, `--replay`, `--raw-out`).
- `scripts/test_blp_fetch.py` -- 21 tests, no Terminal and no `blpapi`
  needed: `python -m unittest discover -s scripts -p "test_*.py"`.
- `tests/fixtures/blpapi/real_*.json` -- five unedited live captures
  (clean EOD, field exception, invalid security, 5-day backfill, bad date).
  Because `--raw-out` writes exactly the format `--replay` reads, any future
  production response can be dropped in as a regression test unchanged.

Only `response_error.json` is synthetic: a request-level `responseError`
could not be provoked on demand, precisely because of Finding A.

### Still outstanding

Open question 1 (**licensing**) is unchanged and remains the one item that
could invalidate this work: the probe proves the API is technically
reachable, not that programmatic use is contractually permitted. Worth the
email before step 10 deletes the Excel path.

---

## 13. Implementation log (2026-08-18)

All 11 steps of §10 are done. The Excel layer is deleted.

### End-to-end proof

`cargo test --test db_integration smoke_real_bloomberg -- --ignored`:

```
smoke: run 15 obs_date=2026-08-17 upserted=2 issues=0
smoke: APPLE INC PX_LAST=305.59 on 2026-08-17
test smoke_real_bloomberg_end_to_end ... ok   (1.90s)
```

Terminal → BLPAPI → PostgreSQL, with `PX_LAST` (HistoricalDataRequest) and
`NAME` (ReferenceDataRequest) landing on the same previous-trading-day
`obs_date`, and the raw response archived. This closes plan task 15, which had
been outstanding since the project began.

### Test counts

| Suite | Before | After |
|---|---|---|
| Rust unit | 39 | 37 |
| Rust integration | 6 (never ran) | 9, **all passing and re-runnable** |
| Python sidecar | — | 21 |
| Frontend `svelte-check` | 139 files, 0 errors | unchanged |

The integration suite went from 6.7 s to 0.5 s (excluding the live test) once
Excel left the path. The unit count dropped by 2 because `excel_gen` and
`excel_read`'s 16 tests were replaced by 14 in `fetch` and `blp_driver`.

### Deviations from the plan as written

1. **`DataFetcher::fetch` takes an `audit_path` argument** that §2.4 did not
   show. The fetcher has to be told where to archive its raw response, and
   threading that through construction would have forced the generic pipeline
   helper into a closure. `MockFetcher` ignores it.
2. **`run_eod` / `run_backfill` each gained a `_with` variant** taking an
   explicit fetcher. The original signatures are preserved as thin wrappers
   that construct a `BlpapiFetcher`, so `commands.rs` and `scheduler.rs` were
   untouched — and tests get their injection point.
3. **The frontend was not entirely untouched**, contrary to §2.3: renaming
   `refresh_timeout_s` → `request_timeout_s` and adding `python_path` needed
   three small edits in `api.ts` and `SettingsScreen.svelte`.
4. **Text fields are omitted from multi-day backfills** (`fetch::plan_requests`).
   Stamping one live reference value across a 30-day range would fabricate
   history. This also fixes the pre-existing Excel bug of pushing text
   mnemonics through BDH, where they always failed.
5. **TimescaleDB became optional**, which §3 did not anticipate. It was not
   available on the PostgreSQL 17 install here — Timescale ships no Windows
   build — so `CREATE EXTENSION` was failing migration 0001 outright and no
   test could run. `observation` is now a plain table unless the extension is
   present; nothing in the schema used a hypertable feature.

### Also fixed along the way

The six integration tests had never run, and could not have passed as written:
`asset_crud_round_trip` and the old pipeline test both inserted the security
`AAPL US Equity` against `UNIQUE (bdp_security)`, so one of them would have
failed on the *first* run. Every fixture name is now uniquified per run and
per process, making the suite repeatable.

### Still outstanding (unchanged by this work)

- No autostart or tray icon; the scheduler only ticks while the app runs.
- No alerting on a failed or persistently `partial` run.
- No automatic catch-up for missed days.
- No backend concurrency guard (the in-flight check is UI-only).
