# P9 + P10 Design: Multi-Asset Capability & Production Operations

**Status:** approved design, not yet implemented.
**Source:** phases P3 and P4 of the 2026-08-20 senior-AM tool assessment. The assessment's
P1+P2 shipped as internal wave P7 (quality gate + currency dimension, merged 2026-08-21)
plus the P8 follow-up (verify run kind, currency belief backfill). The assessment's
numbering collides with internal wave names (P3 = corp actions, P4 = adjustment engine),
so: **assessment P3 → wave P9**, **assessment P4 → wave P10**.
**Plans:** `docs/superpowers/plans/2026-08-21-p9-multi-asset.md` and
`docs/superpowers/plans/2026-08-21-p10-production-ops.md`. P9 and P10 are independent;
either can ship first. Within each wave, tasks are ordered by dependency.

## Why

The pipeline treats every instrument like a cash equity: every EOD run burns 2 corp-action
hits per member even for bonds/indices/FX where the answer is always empty; a yield series
would go through the split-factor engine; the merger stitcher only knows ratios (wrong for
futures rolls, which splice by difference); a machine that was off for three days silently
never fetches the missing days; holidays are only known after the fact from `no_data`
evidence; the hit ledger records pre-flight estimates (and double-counts corp actions);
downstream consumers must understand bitemporality to read a price; connection endpoints
are hardcoded; and no CI exists, so the 180-test Postgres suite only runs on one machine.

## P9 — Multi-asset capability

### 9.1 Per-asset-class capability flags (migration 0011)

Four new columns on `asset_class`, defaults chosen so every existing class keeps today's
equity-shaped behaviour (no data migration needed):

| column | type | default | meaning |
|---|---|---|---|
| `corp_actions_capable` | `BOOLEAN NOT NULL` | `TRUE` | class participates in the automatic per-run corp-action refresh |
| `ma_capable` | `BOOLEAN NOT NULL` | `TRUE` | dead instruments of this class get the M&A investigation; `FALSE` = cap-and-retire path |
| `adjustment_style` | `TEXT NOT NULL CHECK IN ('factors','none')` | `'factors'` | `'none'` = the corp-action factor engine never touches this class (yields, indices, futures) |
| `qc_stale_days_default` | `INTEGER CHECK (IS NULL OR >= 2)` | `NULL` | class-level staleness window used when `field_def.qc_stale_days` is NULL (weekly-NAV funds) |

Enforcement points (each is one task in the plan):

- **Corp actions** — both the pre-run estimate (`orchestrator::corp_actions_estimate`,
  orchestrator.rs:374) and the refresh itself (`corp_actions::refresh_view`,
  corp_actions.rs:322) filter members through
  `JOIN book_entry be … JOIN asset_class ac ON ac.id = be.asset_class_id AND ac.corp_actions_capable`.
  The two queries MUST change in the same commit — the gate's number and the seam's charge
  count the same instruments. This is the direct hit saving: a 50-bond view stops burning
  100 hits/run.
- **Adjustment** — `adjust::adjusted_series` (adjust.rs:104) looks up the instrument's
  class style; `'none'` short-circuits to `adjusted = raw`, `factors_used = 0`, and skips
  the corp_action query entirely. `stitch::stitched_series` inherits this because it reads
  through `adjusted_series`.
- **Quality staleness** — the gate's per-field config query (quality.rs:164-167) becomes
  `COALESCE(f.qc_stale_days, ac.qc_stale_days_default)`. Field-level explicit value always
  wins. No Rust logic change beyond the query.
- **Lifecycle** — `lifecycle::investigate` (lifecycle.rs:148) checks `ma_capable` before
  calling `ma_deals`. `FALSE` → new `retire_path`: refresh identity (so `INACTIVE_DATE`
  caps the series via the existing death-cap machinery, engine.rs:158 / store.rs:328),
  record a `lifecycle_retired` issue, no link, no M&A hits. This is also the
  call/redemption story for bonds (9.3): a called bond gets `INACTIVE_DATE` from Bloomberg
  and ends its series exactly like a delisted equity — no successor, so `plan_chain`
  returns `ChainStop::End` and stitching needs nothing.

UI: a new "Asset classes" section in SettingsScreen editing the four flags, via a new
`update_asset_class_capabilities` command (registry.rs already has
`list_asset_classes`/`create_asset_class`).

### 9.2 Futures rolls: `'roll'` link type with difference splice (migration 0012)

A futures roll junction is additive, not multiplicative: the back contract trades at a
spread to the front, and splicing by ratio distorts every earlier price. Design:

- `instrument_link.link_type` CHECK gains `'roll'`; new nullable column
  `roll_offset DOUBLE PRECISION` with `CHECK (roll_offset IS NULL OR link_type = 'roll')`.
  Semantics: **successor = predecessor + roll_offset** at the junction. Signed, zero allowed.
  (`exchange_ratio` has `CHECK (> 0)` so it cannot carry a signed difference — hence a
  sibling column, mirroring how 0006 added `exchange_ratio` next to `terms`.)
- The stitch composer (stitch.rs:221-276) becomes **affine**: track `(mul, add)` with
  `value = raw * mul + add`. Crossing a ratio junction: `mul *= ratio`. Crossing a roll
  junction with offset `s`: `add += s * mul` (a ratio junction *nearer the target* must
  scale offsets from deeper junctions too). Current behaviour is the special case `add = 0`.
- Junction precedence for `'roll'`: volume series → offset 0 (concatenate unscaled, same
  as today's ratio-1.0 volume rule); asserted `roll_offset` if present; else derive
  `offset = succ_val − pred_val` from the same two-sided junction observations the ratio
  fallback uses (safer than the ratio fallback — no divide-by-zero guard needed).
- `plan_chain` and `has_confirmed_predecessors` need no change (only `'spinoff'` is
  excluded from walking). The P7 cross-currency guard applies unchanged and is exactly
  right for rolls: an additive spread is currency-denominated.
- `SegmentInfo` gains `offset: Option<f64>` (per-junction, like `ratio`); frontend segment
  display shows it.
- **Creation is manual**: a new `create_roll_link` command (predecessor, successor,
  effective date, optional offset) that proposes the link with `link_type='roll'`,
  evidence `{"source":"user"}`, and confirms it immediately with `confirmed_by='user'` —
  the human typing it is the P0 7.2 confirmation gate. Auto-detection of rolls from
  Bloomberg futures-chain data is out of scope for P9.

### 9.3 Fixed income

- **Clean/dirty/accrued are just fields** — the fetch path is entirely view-driven
  (`field_def` per asset class), so `PX_DIRTY_MID`, `INT_ACC`, `YLD_YTM_MID` need zero
  pipeline code. What they need is the right class configuration: `adjustment_style='none'`
  (a yield must never meet the split engine), `qc_nonpositive` left FALSE on yield fields
  (yields legitimately go negative), `corp_actions_capable=FALSE`.
- **Call/redemption awareness** = the 9.1 retire path plus the existing `INACTIVE_DATE`
  death cap. No new lifecycle detection: `stale_candidates` (lifecycle.rs:68) is already
  asset-class-agnostic and `MARKET_STATUS` answers for any security.
- Deliverable: `docs/asset-class-playbook.md` — the recommended capability flags, fields,
  and QC settings per class (equity / fund / index / FX / future / fixed income), so
  configuring a new class is a lookup, not archaeology.

## P10 — Production operations

### 10.1 CI (GitHub Actions)

`.github/workflows/ci.yml`, two jobs: **rust** (ubuntu-latest, postgres:17 service
container, Tauri Linux system deps, `cargo test --no-fail-fast` then
`cargo test --no-fail-fast -- --ignored --skip smoke_real_bloomberg` with
`BLOOM_TEST_DATABASE_URL` pointed at the service and a `createdb bloom_test` step) and
**frontend** (`npm ci` + `npm run check`). The only test needing a live Terminal is
`smoke_real_bloomberg_end_to_end` (db_integration.rs:427) — skipped by name.
`tests/db_integration.rs::test_url()` has no URL fallback, so the env var is mandatory in
CI anyway. Ships first in the P10 plan so the rest of the wave lands under CI.

### 10.2 Honest hit ledger

Today `orchestrator::execute` records the **pre-flight estimate** into `hit_ledger`
(orchestrator.rs:247) — a number that includes the corp-action estimate — while
`BlpapiMasterFetcher::charge()` charges corp actions *again* at the wire seam
(master_fetch.rs:456). So the ledger double-counts corp actions, and never reflects what
was actually dispatched (e.g. text fields are dropped from multi-day requests by
`plan_requests` but still counted by the estimate).

Fix: new pure `fetch::dispatched_hits(specs, start, end)` — Σ per RequestSpec of
securities × fields × (weekdays in range for history; 1 for reference). `execute` records
*that* under the run; the seam keeps charging corp actions; `run.estimated_hits` keeps the
gate estimate (the name is honest). Result: each hit appears in the ledger exactly once,
from the path that dispatched it. Plus a `budget_today` command and a RunScreen line
("hits today X / soft limit Y") so usage is visible without psql.

**Standing decision (2026-08-20, reconfirmed): NO hard cap.** The soft limit warns and
`HardConfirm` gates at 2× soft; nothing ever blocks unconditionally. The assessment's
"true hard cap tied to the 500k licence" is explicitly dropped.

### 10.3 Auto-backfill after downtime

`scheduler::tick` only ever targets `previous_weekday(today)`; `detect_gaps`
(scheduler.rs:286) exists but only the UI calls it. Design: when a schedule fires (due,
nothing ran yet), first call `orchestrator::run_gap_backfill(pool, cfg, view_id, today)`:

- `detect_gaps(pool, view_id, GAP_LOOKBACK_DAYS=10, previous_weekday(obs_date))` — gaps
  strictly *before* the day today's EOD will fetch (else yesterday always looks like a gap).
- Estimate the total; if `budget::check_level` says `Ok`, run each gap range as
  `kind='backfill', trigger='scheduled'` with the gap's instrument. If `SoftWarn`/
  `HardConfirm`, run nothing and report "N gap-days need manual confirmation" into
  `schedule.last_result` — a scheduler cannot click a confirm box (same doctrine as verify).
- At most one auto-backfill attempt per day (a scheduled backfill run already started
  today, any status, suppresses another).
- Then the normal EOD/verify decision proceeds — `already_ran_today` counts only
  `eod`/`verify`, so the gap run never suppresses the day's main run.

Note: migration 0009's retag ("scheduled backfill ⇒ verify") was a one-time historical
correction; new scheduled backfills from this feature are correctly kind `'backfill'`.

### 10.4 Non-trading-day certainty (+ overrides plumbing)

Today a single-day EOD run cannot distinguish a holiday from unexplained silence unless
Bloomberg volunteers a `no_data` problem — `detect_gaps` then proposes backfills for days
that have nothing to fetch (which 10.3 would re-attempt daily). Design, two parts:

- **Overrides plumbing**: `RequestSpec` gains `overrides: Vec<Override>` serialized as
  `[{"fieldId": …, "value": …}]` — the sidecar already validates and applies exactly that
  shape (blp_fetch.py:163-166, :479-491); only the Rust side lacks it.
- **NIL-fill evidence**: the sidecar's historical requests switch from
  `nonTradingDayFillOption=ACTIVE_DAYS_ONLY` to `NON_TRADING_WEEKDAYS` +
  `nonTradingDayFillMethod=NIL_VALUE`; a returned row that has a date but no field values
  is emitted as a `no_data` problem for that (security, date). Rust needs **zero** change:
  ingest Rule A (ingest.rs:182-187) already converts dated `no_data` problems into
  `non_trading_day` rows, quality's completeness check counts them as explained, and
  `detect_gaps` stops proposing them.
- Per-exchange `CDR` calendar override: **deferred** — needs a live entitlement/coverage
  probe on the Terminal (same procedure as the 2026-08-20 fund-merger API probe). The
  plumbing above makes it a one-line addition per request group once probed.

### 10.5 `current_eod` SQL view (migration 0013)

For downstream consumers (risk system, notebooks) that should not need to understand
bitemporality: `CREATE VIEW current_eod` = current belief (`system_to='infinity'`), raw
layer, EOD granularity, joined to `field_def.mnemonic` and `book_entry.label`, exposing
`instrument_id, label, mnemonic, obs_date, value_num, value_text, currency, run_id,
believed_since`. Read-only by construction; adjusted/stitched series remain app-level
(they are mode-parameterised and cannot be one view).

### 10.6 Connection settings in the UI

`AppConfig` gains `database_url`, `blp_host`, `blp_port` (all optional; old `config.json`
files parse unchanged). Precedence for the DB URL: config.json (UI-set) → `BLOOM_DATABASE_URL`
env → hardcoded default; takes effect on restart (the pool is built at startup — the UI
says so). Bloomberg host/port ride the sidecar payload — blp_fetch.py already reads
`payload["host"]`/`payload["port"]` (blp_fetch.py:521-522); only Rust never sends them.
Both `SidecarPayload` (fetch.rs:111) and the master-fetch payload gain the optional fields.

## Out of scope (explicit)

- Hard budget cap — standing user decision, see 10.2.
- Roll auto-detection from futures chains; total-return / holdings layers.
- FX conversion at stitch junctions (the P7 guard's refusal stands).
- Per-exchange CDR calendar codes (10.4 defers to a live probe).
- A JS test runner; frontend verification stays `npm run check`.

## Open questions (resolve before or during implementation)

1. **NIL-fill semantics on live data**: does `NON_TRADING_WEEKDAYS`+`NIL_VALUE` mark
   suspended-but-open days as non-trading? Verify on the Terminal during the P10 smoke
   (compare a known exchange holiday vs. a halted stock) before trusting Rule A rows from it.
2. **Roll-link creation UX**: P9 ships a minimal numeric form in InstrumentDetail;
   a search-picker is a later polish item.
3. **CDR probe**: which calendar codes are entitled, and does the override apply per
   security or per request batch? Probe live, then decide grouping.
