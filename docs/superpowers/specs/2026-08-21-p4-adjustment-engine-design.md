# P4 — Adjustment engine (design)

**Date:** 2026-08-21
**Status:** IMPLEMENTED 2026-08-21 (plan: ../plans/2026-08-21-p4-adjustment-engine.md)
**Branch:** `bloomberg-security-master` (merged to master 2026-08-21; work continues here)
**Depends on:** P3 design `2026-08-20-p3-corporate-actions-design.md` (the
factor chain is stored and refreshed with every run), P0
`2026-08-19-blpapi-field-facts.md` §10.1 (operator/flag semantics).

## 1. Objective

The user's founding requirement: store RAW prices, keep corporate actions
beside them, and **visualise raw or net prices on accurate timeseries** —
the DPDF idea. P4 delivers the missing last step: adjusted series **derived
on read** from `observation` (layer `raw`, current rows) and `corp_action`
(current factor rows). Nothing is ever stored: an amended factor changes
every future read automatically, and the stored history stays pure fact.

## 2. Modes (the DPDF settings, reduced to what the data supports)

- **Raw** — the stored values, untouched.
- **Splits** — apply only factor rows with `flag = 3` (capital changes:
  prices AND volumes). Comparable across splits, dividends still visible.
- **All (net)** — apply every factor row (flag 3 + flag 1 cash dividends).
  The "net price" series the user asked for.

The filtered factor call stores NORMAL_CASH, ABNORMAL_CASH and
CAPITAL_CHANGE in one chain; flag distinguishes capital changes (3) from
cash (1), but normal vs abnormal cash cannot be told apart in the factor
table — so there is no "normal cash only" mode. Honest menu over fake
granularity.

## 3. Semantics (P0 §10.1, measured)

For an observation dated `d`, apply **in chronological order** every usable
factor event with `event_date > d`:

- prices: operator 1 → `p / f`; operator 2 → `p * f`; operator 3 → `p + f`.
- volumes: the OPPOSITE — operator 1 → `v * f`; 2 → `v / f`; 3 → `v - f`.

A series is a volume series when its field mnemonic contains `VOLUME`
(upper-cased check); everything else numeric is treated as a price series.
Volume series only ever receive flag-3 events (flag 1 means "prices only"),
in every mode. Chronological application matters only when operator 3
(additive) mixes with multiplicative operators — rare, but the order is
pinned by a test rather than assumed away.

Same-day twins (occurrence-suffixed keys, live RMS FP evidence) are two
real events: both apply. Factor rows missing any of event_date / amount /
operator / flag (unparsed payloads) are **excluded and counted** in the
result (`unusable_factors`) — a chain we cannot read must not silently
half-adjust a series.

Checks against measured data: AAPL 2020-08-31 factor 4.0, operator 1, flag
3 → pre-split prices divided by 4, volumes multiplied by 4. RMS 2025-05-05
factor 0.994902, operator 2, flag 1 → pre-ex-date prices multiplied by
0.994902 in All mode, untouched in Splits mode.

## 4. Shape

- `src-tauri/src/adjust.rs`: a pure `apply_chain` (unit-testable without a
  database) + `adjusted_series(pool, instrument_id, field_id, mode, limit)`
  loading current raw observations and current factors.
- Command `list_adjusted` + `export_adjusted_csv` (columns
  `obs_date,raw,adjusted`), same typed-path pattern as the other exports.
- Data tab: a **Series** selector (Raw / Split-adjusted / Split + dividend
  (net)); non-Raw adds an Adjusted column and switches the CSV export. A
  note states the series is derived on read from the stored factor chain.

## 5. Non-goals

- No stored adjusted observations, ever (founding P2/P4 rule).
- No total-return index (reinvested dividends) — a later derivation if
  wanted; the net-price series is what DPDF shows.
- No fund-merger stitching (P5).
