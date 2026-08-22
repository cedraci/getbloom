# P11 Design: Publication Cadence & Fetch Capability

**Status:** approved design, not yet implemented.
**Source:** the 2026-08-22 post-P10 assessment, finding 4 ("the weekday-daily heartbeat
does not support the claimed asset universe"), deepened at the user's request and
grounded by the 2026-08-22 live probe (below). P9 gave asset classes the ability to
*switch off* equity behaviours; P11 gives the pipeline what other asset classes
actually need — a model of **when data is supposed to exist** and **which wire path
can fetch it**.
**Plan:** `docs/superpowers/plans/2026-08-22-p11-cadence-and-fetch-capability.md`.
**Migration stance (user ruling 2026-08-22):** the tool is still in a test
environment and current DB contents are disposable. Migration 0014 is plain DDL —
defaults may be chosen freely, no data-preservation constraints. (It still ships as a
NEW migration file: sqlx replays the whole chain on fresh databases, and old migration
files are never edited — standing rule.)

## Why

The system has no concept of a series' expected publication frequency, so "no value
yesterday" is uninterpretable — holiday, suspension, fetch failure, maturity, missing
entitlement, and "this fund publishes monthly" all look identical. Today the code
resolves that ambiguity one way for everyone: daily-weekday-print, holiday-shaped.
Consequences, each verified against the code:

- `blp_fetch.py:457` hardcodes `periodicitySelection: DAILY`; there is no way to ask
  Bloomberg for anything else.
- `scheduler::tick` fetches every active view every weekday for the previous weekday
  (`scheduler.rs:123,195`); a monthly-NAV vehicle burns ~21 fetch-days per real data
  point — ~95% waste, forever.
- `detect_gaps` expects every non-text field present on every weekday
  (`scheduler.rs:253,360`); one non-daily field per instrument makes a date permanently
  "uncovered", which arms the scheduler's auto-backfill to re-buy the same 10-day
  window **every day** (the known "permanently-partial days" defect from the P10 final
  review — a non-daily instrument converts that bounded leak into a guaranteed one).
- Rules A/B (`ingest.rs:153`) turn NIL evidence into `non_trading_day` rows whose
  defined meaning is "Bloomberg said there was no session". Applied to a monthly
  series that meaning is a lie (~240 fake holidays/year/fund); applied to an
  unentitled series it is a silent catastrophe (see probe finding F6).
- Verify re-reads the trailing 5 weekdays (`scheduler.rs:100`) — the window
  structurally misses monthly prints, and NAV restatement is precisely the correction
  mode bitemporal supersession exists to capture.
- A matured bond or delisted instrument is never noticed: `retire_path` (P9) only
  fires on an identity observation that nothing ever fetches (the deferred P9
  explicit-identity-fetch rider). Every matured bond becomes a zombie burning budget
  daily.

## Live probe, 2026-08-22 (~300 hits, all findings against the real Terminal)

Probe set (user-supplied): SPX Index, AAPL UW Equity, USGG10YR Index,
"T 2 3/8 05/15/31 Govt", EUR Curncy, CL1 Comdty, XAU BGN Curncy. Window for holiday
discrimination: 2026-07-01..10 (contains Fri 2026-07-03, US Independence Day observed —
equity/bond/NYMEX closed, FX and spot metals open). Raw captures are committed as
sidecar test fixtures:
`src-tauri/scripts/fixtures/live-2026-08-22-nilfill-multiasset-history.json` (F1) and
`live-2026-08-22-bond-allnil-history.json` (F6).

| # | Finding | Consequence for design |
|---|---|---|
| F1 | NIL-fill **works as designed, live-confirmed for the first time**: SPX/AAPL/CL1 each returned a NIL row for 2026-07-03 which the sidecar turned into `no_data` / "non-trading day (NIL fill)". EUR and XAU printed real values that day. No weekend rows appear. | P10's biggest untested surface is now verified; the captures become permanent canned tests. Per-security calendars (not per-market) confirmed as the right evidence model. |
| F2 | USGG10YR printed on 2026-07-03 **with 2026-07-02's exact value** — benchmark indices carry values across underlying-market holidays. | Repeated values on/around holidays are normal for benchmarks; QC staleness must not be tightened for this class. No code change required, recorded as doctrine. |
| F3 | `periodicitySelection: MONTHLY` returns exactly one row per month, dated the month's **last trading day**; the in-progress month is **absent** (no partial row). QUARTERLY behaves the same at quarter-ends. NIL-fill options are accepted but **inert** under MONTHLY (no NIL month-rows). | Monthly cadence planning can rely on: print exists only after month-end; absence of the current month is normal, not a gap. Sidecar may leave fill options set, but setting them only for DAILY is cleaner and is what we do. |
| F4 | `calendarCodeOverride` is accepted and **changes NIL semantics**: with `JN`, 2026-07-03 comes back as a NIL row; with `US`, the day is **omitted entirely** — the override would erase the very evidence rows Rule A consumes. | CDR overrides are **rejected for P11** (they would silently destroy non-trading evidence). Closes spec Open Question 3 of the P9/P10 design: answer is "entitled, but do not use with NIL-fill". |
| F5 | Identity fields are class-dependent: MARKET_STATUS exists for SPX/AAPL (ACTV) but is N/A for FX/commodity/rates-index. CL1 exposes LAST_TRADEABLE_DT (2026-09-22). EUR/XAU spots report SECURITY_TYP=SPOT with **MATURITY = the rolling T+2 settlement date**. | The identity sweep (11.8) must select fields per class, and **MATURITY must never drive retirement for spot classes** — it would retire every FX pair two days after onboarding. |
| F6 | Individual govt bonds: coupon-style tickers do not resolve on this setup (even the real current 10Y "T 4 5/8 08/15/36 Govt" → invalid); CT10/GT10 generics and `/isin/US91282CRF04` resolve; `@BGN` is accepted — but **historical PX_LAST is NIL for every weekday, every addressing form** (8/8 days, including genuine trading days). This licence has no historical pricing entitlement for individual govvies. Today's Rules A/B would record all eight days as `non_trading_day` evidence, suppressing the gap forever: **an entitlement hole is indistinguishable from a holiday and produces zero alerts.** | Two design elements: `fetch_via = 'reference'` capability (11.2) so history is never requested where it cannot succeed, and the NIL-streak quality finding (11.6) so an all-NIL series screams instead of self-silencing. |
| F7 | The **reference path returns live bond prices**: CT10 Govt PX_LAST=99.140625, PX_BID, YLD_YTM_MID=4.7349 all populated. | Individual bonds ARE collectable daily on this licence — via `kind: reference` snapshots, not history. They can never be backfilled (missed day = permanent hole, by entitlement, not by design). |
| F8 | `//blp/instruments` SECURITY search returns nothing for treasury queries (govt securities need the unimplemented `govtListRequest`). | Bond onboarding guidance: enter `/isin/<ISIN>` or CT/GT generic tickers directly. `govtListRequest` is out of scope. |

## 11.1 Cadence model (migration 0014)

Publication cadence is a property of the series. Class default plus per-field override:

| column | on | type | default | meaning |
|---|---|---|---|---|
| `default_cadence` | `asset_class` | `TEXT NOT NULL CHECK IN ('daily','weekly','monthly','quarterly','irregular')` | `'daily'` | expected publication frequency for the class's fields |
| `cadence` | `field_def` | same CHECK, NULL allowed | `NULL` | overrides the class default (a RE fund's daily market price vs its monthly NAV) |
| `cadence_grace_days` | `asset_class` | `INTEGER NOT NULL CHECK (>= 0)` | `10` | calendar days after a period ends before the missing print is anomalous |

Effective cadence = `COALESCE(field_def.cadence, asset_class.default_cadence)` — the
same COALESCE idiom as `qc_stale_days` (P7/P9).

Semantics:
- `daily` — today's behaviour, bit-for-bit. The entire existing pipeline is the
  daily case; nothing about it changes.
- `weekly` / `monthly` / `quarterly` — expect ≥1 print per period. Probe F3: the
  print is dated the period's last trading day and appears only after the period
  ends. A period is *late* when `today > period_end + grace`.
- `irregular` — collect opportunistically (fetch with the daily partition, keep
  whatever arrives), never gap-detect, never write non-trading evidence, staleness
  QC only. For capital-account-style series that resist period modelling.

## 11.2 Fetch capability: `fetch_via` (migration 0014, same file)

Probe F6/F7: some series are collectable only as reference snapshots. New column:

| column | on | type | default | meaning |
|---|---|---|---|---|
| `fetch_via` | `field_def` | `TEXT NOT NULL CHECK IN ('history','reference')` | `'history'` | which wire path collects this field |

- `'history'` — ranged HistoricalDataRequest, today's behaviour.
- `'reference'` — the field joins the EOD run as a ReferenceDataRequest snapshot
  dated `obs_date` (the pipeline's existing reference plumbing — text identity
  fields already travel this way). Consequences, all enforced:
  - **No backfill, ever**: `plan_backfill`/gap requests exclude reference fields the
    same way they exclude text fields today (`detect_gaps`' rationale extends
    verbatim: "backfill cannot recover them by design; an unfixable gap is noise").
  - **No non-trading evidence**: a reference snapshot with no value proves nothing
    about sessions.
  - **Verify cannot re-read the past** for these fields; they are excluded from the
    verify window (the daily snapshot itself is the freshest obtainable truth).
  - A missed day is a permanent hole and the UI says so (11.9): that is the honest
    shape of this licence's bond entitlement, not something to paper over.
- Caveat, stated openly: a reference snapshot taken at the scheduled run time is
  **today's live/latest value, not an official close**. For CT10 at 18:00 Paris that
  is close-adjacent but not identical. `observation.layer`/basis do not change;
  the field's own definition documents the semantic. Users who need true closes for
  bonds need entitlement, not code.

## 11.3 Sidecar: periodicity passthrough

`RequestSpec` grows `periodicity: Option<String>` serialized only when set
(`skip_serializing_if = "Option::is_none"` — the P10 host/port discipline: wire bytes
for existing daily requests do not change). `blp_fetch.py`:

- `build_request` history branch: `r.set("periodicitySelection", spec.get("periodicity") or "DAILY")`.
- The NIL-fill pair (`nonTradingDayFillOption`/`Method`) is set **only when the
  effective periodicity is DAILY** (probe F3: inert elsewhere; conditional keeps the
  contract explicit).
- Adjustment-flag block unchanged for all periodicities.
- Parsing is untouched — monthly rows arrive in the same `fieldData` shape (probe F3).

P0 doctrine holds: canned-response tests in `test_blp_fetch.py` for (a) request
building with/without periodicity, (b) replay of the two committed live fixtures —
the multi-asset NIL capture (F1) must reproduce exactly the 45 observations and 4
problems seen live; the bond all-NIL capture (F6) exactly 0 observations / 25 problems.

## 11.4 Planner: fetch when due, not daily

`run_eod`'s planning partitions the view's (instrument, field) pairs by
(effective cadence, fetch_via):

- **daily × history** — unchanged (the entire current path).
- **daily × reference** — snapshot dated `obs_date`, batched into one
  ReferenceDataRequest per run alongside the existing text-field reference leg.
- **periodic × history** (weekly/monthly/quarterly) — included in a run only when
  **due**: the most recently *ended* period has no current observation AND no fetch
  for that (instrument, field, period) has already been dispatched today. When due,
  the request is one ranged history fetch covering the whole period with the matching
  `periodicity` (one expected row, per F3), re-attempted at most once per day until
  the print lands or the period leaves the lookback. Not-yet-ended periods are never
  fetched (F3: no partial rows exist).
- **irregular × history** — rides the daily partition unchanged (opportunistic).

`dispatched_hits` grows a periodicity-aware branch: a periodic history request costs
`securities × fields × periods_between(start, end)` (weekday counting stays for
daily). Reference legs already cost `securities × fields × 1`.

Budget effect: a monthly instrument drops from ~21 fetch-days per print to 1-3
(the retries inside grace) — roughly 90% fewer hits per non-daily instrument, which
is what makes holding many PE/RE lines viable under the 500k/day licence.

## 11.5 Gap detection & the partial-day re-buy fix

`detect_gaps` becomes cadence-dispatching:

- **Daily fields**: exactly today's `missing_weekdays` logic — with the coverage
  predicate (`scheduler.rs:360`) counting **only daily×history fields** toward
  `need`. Text fields stay excluded (already are); reference and periodic fields
  join them, for the same written reason. This single change removes the amplifier
  on the P10-review "permanently-partial days" defect: a date can no longer be
  permanently uncovered because of a field that daily backfill could never supply.
- **Periodic fields**: a gap is **period-shaped** — "period P has no print and
  `today > period_end + grace`". Lookback = 2 completed periods (not
  `GAP_LOOKBACK_DAYS`, which is meaningless at monthly scale). Its backfill is the
  same single ranged request as 11.4. `Gap` gains an optional period label so the UI
  can render "2026-07 missing" instead of a fake day-range.
- **Irregular / reference fields**: never gap-detected.

The scheduler's once-per-day attempt cap and the `BudgetLevel::Ok`-only gate are
unchanged and cover both gap shapes.

## 11.6 Evidence honesty: gating + the NIL-streak alarm

Two rules, both born from probe F6:

1. **Gating**: Rules A and B (`ingest.rs`) write `non_trading_day` only for
   fetches of **daily-cadence, history-via fields**. Periodic fetches (absence
   inside a period is expected), irregular fields, and reference snapshots write
   nothing. The table keeps exactly one meaning: "Bloomberg said this instrument had
   no session that day."
2. **NIL-streak finding**: at ingest, if an instrument's trailing run of consecutive
   weekdays that are all NIL/non-trading (current fetch plus stored evidence)
   reaches **5**, emit a `severity='quality'` ingest issue, code `nil_streak`,
   detail naming the span. Real calendars never produce 5 consecutive closed
   weekdays in the probed markets; a suspension legitimately does (and deserves the
   flag); a missing entitlement always does (and MUST get it — F6 shows this failure
   is otherwise perfectly silent). Evidence is still recorded (so the auto-backfill
   loop stays quiet — deliberate: the alarm is the human's signal, the evidence stops
   the machine from re-buying junk). If entitlement is later fixed, verify/manual
   backfill inserts real values; leftover evidence rows become harmless.

`late publication` for periodic series is also a quality finding: code
`publication_overdue`, raised when a period is late past grace (11.5 detects it);
it replaces gap-noise with the alert that actually matters ("the June NAV never
arrived").

## 11.7 Verify per cadence

The verify slot (P7) re-reads per partition:

- daily×history: trailing 5 weekdays — unchanged.
- periodic×history: the last **2 completed periods**, one ranged request with the
  matching periodicity. NAV restatements land as `value_superseded` warns exactly
  like price restatements — this is the single highest-value change for PE/RE data
  quality.
- reference-via and irregular fields: excluded (nothing past to re-read).

## 11.8 Identity sweep — the P9 rider, designed at last

A weekly scheduled sweep (rides the existing verify-day slot machinery; one drawn
time, one attempt) that batches ONE ReferenceDataRequest per asset class over the
class's active instruments and feeds existing plumbing. Class capability column
(migration 0014):

| `identity_sweep` value | fields fetched | retirement trigger |
|---|---|---|
| `'none'` (default for FX/spot/generic-future/index classes) | — | never (F5: spot MATURITY is a settlement date; generics roll) |
| `'market_status'` (equities, funds) | MARKET_STATUS, INACTIVE_DATE | `MARKET_STATUS <> 'ACTV'` or INACTIVE_DATE set → existing `retire_path`/M&A path per `ma_capable` |
| `'maturity'` (bonds) | MATURITY, CALLED_DT, INACTIVE_DATE | any of them ≤ today → `retire_path` |

Hits are charged at the wire seam via `budget::record_purpose_hits`
(purpose `'identity'`, run_id NULL) — the corp-actions precedent, same seam, same
no-double-count guarantee. Cost: ~2-3 hits per instrument per week. This kills the
zombie problem: matured/called/delisted paper retires itself within a week instead
of burning budget indefinitely.

## 11.9 UI & onboarding

- Settings → Asset classes: `default_cadence`, `cadence_grace_days`,
  `identity_sweep` columns join the P9 capability editor.
- Field editor: `cadence` override and `fetch_via` selectors; choosing
  `'reference'` shows the plain-words warning: "snapshot at run time, not an
  official close; missed days cannot be backfilled".
- Book/onboarding hint for bonds (F6/F8): "enter `/isin/<ISIN>` or a CT/GT generic —
  coupon-style tickers do not resolve, and instrument search does not cover
  government bonds."
- Gap report renders period-shaped gaps as their period label.

## Out of scope (explicit)

- Intraday granularity (schema slot exists; nothing else).
- Weekend-trading markets (crypto, Gulf exchanges): `is_weekend` stays global.
  **Documented exclusion** — previously it was an accident.
- Bond corporate events beyond retirement (calls-as-adjustments, coupon schedules).
- `govtListRequest` support in the sidecar (F8; `/isin/` addressing suffices).
- CDR `calendarCodeOverride` (F4: destroys NIL evidence; rejected, not deferred).
- Per-instrument cadence overrides (YAGNI until two same-class instruments prove
  to differ).
- Hard budget cap — standing user decision 2026-08-20, still standing.

## Open questions (resolve before or during implementation)

1. **A real monthly-NAV fund ticker is still needed.** The user's probe set contained
   no fund; monthly behaviour is verified on SPX MONTHLY (F3) but a real NAV
   series' print-date pattern (mid-month publication lag? dated month-end or
   publication day?) is unverified. The first fund onboarded must be watched for its
   first two cycles, and `cadence_grace_days` tuned to its observed lag. Ask the
   user for one fund + one SCPI ticker they actually hold.
2. **Reference-snapshot timing for bonds** (11.2 caveat): if run-time snapshots
   prove too noisy vs closes, the fallback is scheduling the run later in the
   evening — a schedule-window question, not a code question.
3. Whether `nil_streak`'s threshold of 5 needs a per-class override (a class of
   thinly-quoted munis might legitimately stall longer). Ship the constant, add the
   override only on evidence.
