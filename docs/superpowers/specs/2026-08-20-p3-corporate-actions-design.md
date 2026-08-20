# P3 — Corporate-action ingestion (design)

**Date:** 2026-08-20
**Status:** APPROVED (user, 2026-08-20: "move forward on all your suggestions
except the hard 500k limit")
**Branch:** `bloomberg-security-master`
**Depends on:** `2026-08-19-blpapi-field-facts.md` (P0) §4, §5, §10.1;
`2026-08-19-security-master-design.md` §11 (P3 roadmap entry).
**Scope:** fetch, version and store corporate-action data. **Out of scope:**
applying it — the adjustment engine over the factor chain is P4, and nothing
in P3 writes any `observation.layer` other than what already exists.

## 1. Objective

Store the two datasets P4's adjustment engine and the user's dividend
requirements need, with the same bitemporal honesty the rest of the master
uses:

1. **The factor chain** — `EQY_DVD_ADJUST_FACT` with
   `CORPORATE_ACTIONS_FILTER = NORMAL_CASH|ABNORMAL_CASH|CAPITAL_CHANGE`.
   P0 §10.1: the filtered call returns splits (operator 1, flag 3) *and* cash
   dividends (operator 2, flag 1) in one request — a superset, so one request
   suffices. Column names, measured: `Adjustment Date`, `Adjustment Factor`,
   `Adjustment Factor Operator Type`, `Adjustment Factor Flag` (capture:
   `blpapi-facts/headline_report.json`, `plain::AAPL US Equity`).
2. **Dividend history** — `DVD_HIST_ALL_WITH_AMT_STATUS` (P0 §5: prefer the
   `_WITH_AMT_STATUS` variants; they distinguish an estimated from a confirmed
   amount). **No P0 capture exists for its column names.** The parser is
   therefore tolerant: every row's verbatim JSON is stored, typed columns are
   extracted through a candidate-name map, and a row whose expected keys are
   absent produces an `ingest_issue` (`code='corp_action_unparsed'`) instead
   of a silent drop. First live run verifies the names; the map is then
   corrected if needed.

## 2. Fetch path

A new `MasterFetcher::corp_actions(security)` method issues ONE
`bulk_reference` request (sidecar kind already implemented and tested) for
both fields with the `CORPORATE_ACTIONS_FILTER` override, and returns the
sidecar's `bulk_rows` (now carried through `SidecarResponse` — the previously
dead wire). Charged at the wire seam like every master request, purpose
`corp_actions`, at securities × fields = **2 hits** per instrument refresh —
the same per-security-field accounting `estimate_eod_hits` and
`identity_hit_cost` use.

Trigger is a **user action**: a "Refresh corporate actions" button on the
instrument detail panel (same pattern as identifier history — explicit,
costed, never automatic). No scheduled fetch in P3; cadence is a P4 question
because only the adjustment engine knows how stale a factor chain can be.

**Shipped 2026-08-21 — view-level refresh.** One stock at a time does not
scale (user requirement), so the seam takes a *batch* of securities
(`MasterFetcher::corp_actions(&[String])`, one `bulk_reference` request per
100 securities, charged securities × 2 hits per request) and
`corp_actions::refresh_view` covers a whole view: members without a security
valid today are skipped and reported (`corp_actions_skipped`), each
instrument diffs its own tables out of the shared response in its own
transaction, and the Views screen carries the per-view "Corp actions"
button. A read-only **Data** tab renders stored observations (with basis and
supersession history) and corporate actions with their verbatim payload,
plus CSV export — the accuracy-check surface.

## 3. Storage — `corp_action`

One table, source-field discriminated, bitemporal on system time only (the
event date IS the valid time; Bloomberg reports the full history on every
call, so there is no separate validity interval to track):

- `natural_key` identifies the event within
  `(instrument_id, source_field)`:
  - factors: `{adjustment_date}|{operator}|{flag}`
  - dividends: `{ex_date}|{dividend_type}` (fallback: the row's canonical
    JSON when either key is missing — such rows are stored, flagged, and
    still diffable)
- refresh = full-snapshot diff against current rows:
  - new key → insert;
  - same key, changed payload → close `system_to`, insert (an amendment —
    e.g. an estimated amount confirmed at a different value);
  - key present locally but absent from the fresh snapshot → close
    `system_to` without replacement (a cancelled action), plus an
    `ingest_issue` (`code='corp_action_withdrawn'`) so it is visible;
  - unchanged → nothing.
- typed columns (`event_date`, `amount`, `operator`, `flag`, `dvd_type`,
  `frequency`, `declared_date`, `record_date`, `pay_date`, `amount_status`)
  are nullable extractions for P4 and the UI; `payload JSONB` is the
  authority. Closing `system_to` is the only permitted UPDATE (trigger, same
  pattern as `observation_append_only`).

## 4. The EOD pipeline refuses bulk fields

`plan_requests` routes by `value_kind` and would coerce a bulk field into a
meaningless string (the sidecar docstring's exact warning). P3 closes the
hole: `load_view` skips fields with `bbg_ftype = 'BulkFormat'` and records an
`ingest_issue` (`code='bulk_field_skipped'`) naming the field and pointing at
the corporate-actions refresh. Skipped, not an error: one misconfigured field
must not kill a 200-instrument run.

## 5. Non-goals, restated

- No derived/adjusted observation layers (P4).
- No automatic or scheduled corp-action fetches.
- No `CA_MA_*` merger evidence fetching (P5 — see P0 §10.3/§10.4).
- No holdings transformation (P5).
