# BLPAPI Field & Schema Facts (P0 discovery)

**Date:** 2026-08-19
**Status:** VERIFIED — every statement below was produced by a live probe against
the Bloomberg Terminal on this machine, not from documentation or memory.
**Purpose:** the factual base every later spec cites, so that the security
master, corporate-actions and adjustment designs use *real* request types,
element names and field mnemonics.

> **Rule this document exists to enforce:** do not invent Bloomberg field names,
> event types, request parameters or adjustment semantics. If a mnemonic is not
> in this document or re-verified by a fresh probe, it is not known to exist.
> Six plausible-sounding mnemonics were guessed during this work and **all six
> do not exist** (§7).

Evidence captures live in `blpapi-facts/`. Probe scripts were throwaway and are
not kept; §8 records how to reproduce each finding.

---

## 1. Environment

| | |
|---|---|
| blpapi Python package | 3.26.7.1 |
| Python | 3.14.6, `C:\Python314\python.exe` |
| Transport | Desktop API, `localhost:8194` |
| Entitlement class | Desktop API (same as the Excel add-in) |

---

## 2. Services — what is reachable

All twelve candidate service URIs opened successfully. `openService` returning
true proves the service is reachable, not that every dataset inside it is
entitled.

| Service | Operations |
|---|---|
| `//blp/refdata` | `ReferenceDataRequest`, `HistoricalDataRequest`, `IntradayBarRequest`, `IntradayTickRequest`, `PortfolioDataRequest`, `BeqsRequest`, `CustomEqsRequest`, `ReferenceDataSlowRequest`, `ReferenceDataSlowMtgeRequest`, `IntradayBarDateTimeChoiceRequest`, `PingRequest` |
| `//blp/apiflds` | `FieldInfoRequest`, `FieldSearchRequest`, `CategorizedFieldSearchRequest`, `FieldListRequest`, `APITerminalFieldSearchRequest` |
| `//blp/instruments` | `instrumentListRequest`, `curveListRequest`, `govtListRequest` |
| `//blp/mktlist` | `SnapshotRequest` |
| `//blp/tasvc` | `studyRequest` |
| `//blp/exrsvc` | `ExcelGetGridRequest` |
| `//blp/irdctk3` | 17 curve-toolkit operations |
| `//blp/mktdata`, `//blp/mktbar`, `//blp/mktvwap`, `//blp/pagedata`, `//blp/srcref` | subscription services, no request operations |

### 2.1 Finding — there is no corporate-actions service

**No service in the reachable set is a corporate-actions service.** Corporate
actions on Desktop API are delivered as **bulk reference fields through
`//blp/refdata`** (§5), not as a dedicated request type or event stream.

Anything in a future design that assumes a CA-specific service, a CA event
subscription, or Data-License-style CA file delivery is wrong for this
entitlement.

### 2.2 `//blp/instruments` is the resolution surface

`instrumentListRequest` takes `query` (keyword string), `yellowKeyFilter`
(`YK_FILTER_NONE | YK_FILTER_EQTY | YK_FILTER_CORP | YK_FILTER_GOVT |
YK_FILTER_INDX | YK_FILTER_CURR | YK_FILTER_MTGE | YK_FILTER_MUNI |
YK_FILTER_PRFD | YK_FILTER_CMDT | YK_FILTER_CLNT | YK_FILTER_MMKT`),
`languageOverride` and `maxResults`. This is the API equivalent of `SECF<GO>`
and is what the security master should use to produce a **candidate list** for
an ambiguous ticker or ISIN.

---

## 3. Raw vs adjusted — the four adjustment flags

`HistoricalDataRequest` carries four booleans, confirmed present in the live
schema (`blpapi-facts/schema_blp_refdata.txt`):

- `adjustmentNormal` — normal cash distributions
- `adjustmentAbnormal` — abnormal/special distributions
- `adjustmentSplit` — splits, stock dividends, rights
- `adjustmentFollowDPDF` — follow the Terminal's `DPDF<GO>` setting

### 3.1 Measured behaviour

`AAPL US Equity`, `PX_LAST`, daily, `ACTIVE_DAYS_ONLY`, around the 4:1 split
with ex-date 2020-08-31:

| variant | 2020-08-28 | 2020-08-31 | 2020-09-01 |
|---|---|---|---|
| all four false | **499.23** | 129.04 | 134.18 |
| `adjustmentSplit` only | 124.8075 | 129.04 | 134.18 |
| normal + abnormal + split | 120.9559 | 125.0578 | 130.0392 |
| `adjustmentFollowDPDF = true` | 120.9559 | 125.0578 | 130.0392 |

Readings:

1. **`499.23` is the true as-traded close.** Raw observations therefore require
   **all four flags explicitly false**. There is no other way to obtain them.
2. `124.8075 = 499.23 / 4` — split adjustment is a pure back-multiplication
   applied only to dates before the ex-date.
3. Dividend adjustment restates **every** date in the window, including the
   ex-date and after (129.04 → 125.0578), because it discounts back from the
   most recent distribution. Adjusted series are therefore **not** stable over
   time: the same historical date changes value as new dividends occur. This is
   the mechanical reason adjusted values must never be stored as if they were
   observations of record.
4. **`followDPDF` matched full adjustment on this Terminal today.** `DPDF<GO>`
   is a *per-user, per-Terminal screen setting*. Left at its default, an
   identical request returns different numbers on another machine, or on this
   one after somebody changes a screen.

### 3.2 Consequence for existing data

The current pipeline (`fetch.rs` / `blp_fetch.py`) sets **none** of the four
flags. Every row now in `observation` is therefore *Bloomberg default,
DPDF-following, with the Terminal setting at fetch time not captured* — not
raw, and not reproducible.

**Decision (user, 2026-08-19):** store the adjustment basis explicitly per
observation rather than marking legacy rows "unknown". Legacy rows record the
exact truth above, which is a granular and queryable fact, and remain
distinguishable from true raw without being discarded.

### 3.3 Independent corroboration

`//blp/refdata` also exposes the paired scalar fields
`UNADJUSTED_PREV_LAST_PRICE` and `ADJUSTED_PREV_LAST_PRICE` (plus `_RT`
variants). Bloomberg itself models raw and adjusted as two distinct products of
the same instrument-day, which is the distinction the design objectives require.

---

## 4. Adjustment factors — `EQY_DVD_ADJUST_FACT`

`ftype = BulkFormat`. Override: `CORPORATE_ACTIONS_FILTER` (id `DV175`).
Returns data with **no overrides required**.

Columns: `Adjustment Date`, `Adjustment Factor`,
`Adjustment Factor Operator Type`, `Adjustment Factor Flag`.

`AAPL US Equity` returned, unprompted and correct:

| Adjustment Date | Adjustment Factor | Operator Type | Flag |
|---|---|---|---|
| 2020-08-31 | 4.0 | 1.0 | 3.0 |
| 2014-06-09 | 7.0 | 1.0 | 3.0 |
| 2005-02-28 | 2.0 | 1.0 | 3.0 |
| 2000-06-21 | 2.0 | 1.0 | 3.0 |

These are Apple's 4:1, 7:1, 2:1 and 2:1 splits on their correct ex-dates.

This is the **factor chain** for internally derived adjusted series: dated,
typed, and auditable, so derived values can be recomputed and explained rather
than inferred from price discontinuities.

`META US Equity` returned a single row, factor 5.0 dated 2010-10-31 — before
its 2012 IPO. The table is not limited to listed history, so a factor chain
must not be assumed to start at the listing date.

**Established in the second pass — see §10.1.** `Operator Type` is
1 = divide, 2 = multiply, 3 = add, **opposite for volume**; `Flag` is
1 = prices only, 3 = prices and volumes. They were constant at 1.0 / 3.0 here
only because splits are the default content of this field; adding
`CORPORATE_ACTIONS_FILTER` brings in cash dividends at operator 2 / flag 1 and
separates the two.

---

## 5. Corporate-action bulk fields

All confirmed present with `ftype = BulkFormat`:

| Mnemonic | Description |
|---|---|
| `DVD_HIST` | Dividend History - Cash |
| `DVD_HIST_ALL` | Dividend History - All |
| `DVD_HIST_ALL_WITH_AMT_STATUS` | Dividend History - All (With Amount Status) |
| `DVD_HIST_WITH_AMT_STATUS` | Dividend History - Cash (With Amount Status) |
| `DVD_HIST_GROSS_WITH_AMT_STAT` | Dividend History (Gross) - With Amount Status |
| `EQY_DVD_HIST_SPLITS` | Dividend History - Splits |
| `DVD_THRESHOLD_SCHEDULE` | Dividend Threshold Schedule |

Accepted overrides include `DVD_START_DT` and `DVD_END_DT` (both confirmed to
exist), and `CORPORATE_ACTIONS_FILTER`.

**Prefer the `_WITH_AMT_STATUS` variants.** They carry whether an amount is
estimated or confirmed, which is the signal objective 7 needs to distinguish a
genuine amendment from a first-time confirmation of a previously estimated
figure.

Scalar split fields: `EQY_SPLIT_DT` (Date), `EQY_SPLIT_RATIO` (Character),
`EQY_SPLIT_ADJUSTMENT_FACTOR` (Real), `SPINOFF_ADJ_FACTOR_CURR`,
`SPINOFF_ADJ_FACTOR_NEXT`.

Type lookups (bulk headers): `BH_LU_CP_STOCK_SPLT_TYP`,
`BH_LU_CP_DVD_STOCK_TYP`, `BH_LU_CP_CALL_TYP`, `BH_LU_CP_DELIST_REASON`,
`BH_CP_REORG_PLAN`.

`ftype == "BulkFormat"` is the machine-readable marker distinguishing
table-valued fields from scalars, and is what the configurable field-mapping
layer should key on rather than a hand-maintained list.

---

## 6. Identity, lifecycle and identifier history

### 6.1 Identity anchors (all confirmed)

| Mnemonic | Meaning |
|---|---|
| `ID_BB_GLOBAL` | FIGI, instrument level |
| `ID_BB_GLOBAL_SHARE_CLASS_LEVEL` | Share-class FIGI |
| `PRIM_SECURITY_COMP_ID_BB_GLOBAL` | Composite FIGI |
| `PRIM_EXCH_FIGI_SHARE_CLASS` | Primary-exchange FIGI within a share class |
| `ID_BB_UNIQUE` | Bloomberg unique identifier |
| `ID_ISIN`, `ID_CUSIP`, `ID_SEDOL1` | Public identifiers |
| `ID_BB_COMPANY`, `ID_BB_GLOBAL_COMPANY` | Issuer level |
| `TICKER`, `TICKER_AND_EXCH_CODE`, `EXCH_CODE`, `COMPOSITE_EXCH_CODE` | Listing |
| `CRNCY`, `CNTRY_ISSUE_ISO` | Currency, country |
| `SECURITY_TYP`, `SECURITY_TYP2`, `MARKET_SECTOR_DES` | Classification |
| `NAME`, `LONG_COMP_NAME` | Names |

Fund/share-class: `FUND_SHR_CLASS_DESG`, `SHARE_CLASS_TYPE`, `FUND_TYP`,
`FUND_NET_ASSET_VAL`, `FUND_TOTAL_ASSETS`, `FUND_BASE_CURRENCY`,
`FUND_ASSET_CLASS_FOCUS`.

### 6.2 Lifecycle (validity periods)

| Mnemonic | Type | Use |
|---|---|---|
| `LISTING_DATE` | Date | valid-from |
| `INACTIVE_DATE` | Date | valid-to |
| `SIMP_SEC_STATUS`, `RT_SIMP_SEC_STATUS` | Character | Simplified Security Status |
| `TRADE_STATUS`, `EXCH_TRADE_STATUS` | Boolean | trading status |
| `MARKET_STATUS`, `EXCH_MARKET_STATUS` | Character | market status |
| `SECURITY_STATUS_VERSION` | String | status version |
| `FINANCIAL_STATUS_INDICATOR` | Character | financial status |
| `FUND_SHARE_CLASS_CLOSURE_DATE` | Date | fund share-class closure |
| `CA_MA_COMPLETE_DT` | Date | M&A completion/termination |
| `REVERSE_MERGER_COMPLETION_DATE` | Date | reverse merger |

### 6.3 `HISTORICAL_IDS_TIME_RANGE` — the alias history

`ftype = BulkFormat`. Overrides: `HISTORICAL_STARTING_IDENTIFIER` (id `ID289`)
and `HISTORICAL_ID_TM_RANGE_START_DT` (id `ID325`).

**Returns nothing without overrides.** It is a lookup you drive, not a table you
harvest.

Columns: `Date`, `Old ID`, `New ID`, `Old Exch`, `New Exch`, `Action ID`,
`Source`.

With `HISTORICAL_STARTING_IDENTIFIER = "FB US Equity"` and start `20120101`,
both `META US Equity` and `FB US Equity` returned the same row:

```
Date 2022-06-09 | Old ID FB | New ID META | Old Exch US | New Exch US
Action ID 228233742 | Source "ID Change"
```

`Action ID` is a Bloomberg-side event identifier — the natural key for detecting
amendments to an identifier change.

### 6.5 CORRECTION (2026-08-19, live) — the anchor is the chain's START, and §6.4 read it backwards

§6.3 and §6.4 are correct about *what was captured* but were **misread when the
design was written**, and the misreading is the more dangerous half.

`HISTORICAL_STARTING_IDENTIFIER` means exactly what it says: the identifier the
chain **starts** from — not the instrument you are asking about. The P0 probe
that produced the committed capture anchored on **`FB US Equity`**, the *old*
ticker. Nothing in §6.3 recorded that, so the design passed the *resolved current
security* instead, which is the chain's **end**.

Measured live, same request in every other respect, `META US Equity` as the
security, range start `20120101`:

| `HISTORICAL_STARTING_IDENTIFIER` | Result |
|---|---|
| `FB US Equity` (the chain's start) | `FB → META`, Action ID 228233742 — Facebook's real rename |
| `META US Equity` (the current identifier) | `META → METV`, Action ID 229098374 — **the Roundhill ETF** |

**The range start date matters just as much.** Anchored on `FB US Equity` but
with the range opened to `20000101`, the answer becomes `FB → (blank)`,
"Delisted", 2002-04-10 — a different company that held `FB` before Facebook
existed. Widening the window pulls in whoever wore the string earlier.

**Consequences.**

1. Anchoring is **necessary but not sufficient**. Both the anchor and the window
   must be right, and getting the anchor wrong does not fail loudly — it returns
   a well-formed chain belonging to someone else.
2. There is a **bootstrap problem with no clean answer**: you cannot know a
   chain's starting identifier until you have fetched the chain. This field
   therefore **cannot discover a rename you do not already know about**. It can
   only confirm one.
3. So a returned row is **evidence, not a fact**. Nothing may write an alias from
   it automatically. A row whose `New ID` is not already an identifier this
   instrument holds must be refused and surfaced for a human, exactly as an
   ambiguous ownership already is.

This does not weaken §6.4 — it strengthens it. The trap is real and is reachable
from *both* directions: omit the anchor and you get another company's chain; pass
the wrong anchor and you get another company's chain. Evidence:
`blpapi-facts/anchor_semantics.json`.

### 6.4 Finding — one omitted override crosses two unrelated instruments

The same field, asked about `META US Equity` **without**
`HISTORICAL_STARTING_IDENTIFIER`, returned instead:

```
Date 2022-01-31 | Old ID META | New ID METV | Action ID 229098374 | Source "ID Change"
```

That is the **Roundhill Ball Metaverse ETF**, which held the ticker `META` until
it renamed to `METV` on 2022-01-31, freeing the ticker for Facebook five months
later.

So: one security string, one field, two override sets, **two different
companies' identity histories**. The requirement that ticker must never be a
primary key is not stylistic — on this field a single omitted override silently
conflates unrelated instruments.

**Mandatory rule:** always anchor this lookup with an explicit
`HISTORICAL_STARTING_IDENTIFIER`, and persist which identifier was used
alongside the returned rows. A stored alias row whose anchoring identifier is
unknown cannot be trusted.

---

## 7. Negative results

Recorded because a negative result is as load-bearing as a positive one.

### 7.1 Mnemonics that do not exist

Guessed during this work, all rejected by `FieldInfoRequest`:

`DELIST_DATE`, `EQY_DELIST_DATE`, `SECURITY_ACT_STATUS`, `MERGER_TARGET`,
`MERGER_ACQUIRER`, `EQY_ACQUIRER_NAME`.

Use `INACTIVE_DATE` and `SIMP_SEC_STATUS` for delisting/status, and the
`CA_MA_*` family for M&A.

### 7.2 There is no successor-security field

A dedicated search across 17 lifecycle-related queries (1,678 distinct fields)
found **no field returning a successor or predecessor security identifier**.
`FED_PRED_*` matches on name only and is entirely bank regulatory-accounting
data ("Fed Predecessor Net Income").

Consequently **successor/predecessor links cannot be read from a single field**.
They must be derived from `HISTORICAL_IDS_TIME_RANGE` (identifier continuity),
the `CA_MA_*` M&A fields, `REVERSE_MERGER_COMPLETION_DATE` and
`FUND_SHARE_CLASS_CLOSURE_DATE`, and confirmed through the manual-review queue.
Any design that assumes Bloomberg hands over the successor link is wrong.

### 7.3 Bloomberg silently accepts impossible dates

Carried forward from the A2 verification log and still true: a
`HistoricalDataRequest` with `startDate = endDate = 20261301` returns an empty
result and **no error**. Requests must be validated before sending.

### 7.4 `no_data` is not proof of a holiday

Also carried forward from A2. A security that resolves but has no data for a day
is byte-identical to a non-trading day. Never infer a market calendar from
`no_data`.

---

## 8. Reproducing these findings

Probe scripts were throwaway. Each finding is reproducible with a short script
against `localhost:8194`:

| Finding | Method |
|---|---|
| §2 service map | `session.openService(uri)` per URI, then `service.toString()` |
| §3 adjustment behaviour | `HistoricalDataRequest`, AAPL `PX_LAST`, 20200825–20200904, four flag combinations |
| §4 factor chain | `ReferenceDataRequest`, `EQY_DVD_ADJUST_FACT`, no overrides |
| §5 CA fields | `//blp/apiflds` `CategorizedFieldSearchRequest`, searchSpec `"corporate actions"` |
| §6.1–6.2 | `//blp/apiflds` `FieldSearchRequest` + `FieldInfoRequest` to confirm each mnemonic |
| §6.3–6.4 | `ReferenceDataRequest`, `HISTORICAL_IDS_TIME_RANGE`, with and without `HISTORICAL_STARTING_IDENTIFIER` |
| §7.1 | `FieldInfoRequest` returns `fieldError` for a nonexistent mnemonic |

`FieldInfoRequest` is the cheap oracle: it confirms or denies a mnemonic and
returns its `datatype`, `ftype`, `categoryName` and accepted `overrides` without
touching security data. **Use it before writing any mnemonic into code.**

Captured evidence in `blpapi-facts/`:

- `services_report.json` — §2
- `schema_blp_refdata.txt`, `schema_blp_apiflds.txt`, `schema_blp_instruments.txt`
- `headline_report.json` — §4, §6.3 field metadata
- `histids_report.json` — §6.3, §6.4

---

## 9. What this base does and does not settle

**Settled:** how to obtain raw observations; that adjusted series are unstable
over time; where corporate actions come from; the identity anchors and
lifecycle dates available; that identifier history is retrievable and how it
must be anchored.

**Open at first writing — see §10, which closes all but one:**

1. ~~`Adjustment Factor Operator Type` and `Adjustment Factor Flag` semantics~~
   — **settled, §10.1.** 1 = divide, 2 = multiply, 3 = add, opposite for volume;
   flag 1 = prices only, 3 = prices and volumes.
2. ~~The `CA_MA_*` family's full membership~~ — **settled, §10.3.** 89 members.
3. Whether Desktop API bulk CA requests are metered differently from price
   requests — **still open, §10.5.** Not exposed by the API. The estimator stays
   calibrated to the Excel add-in's accounting and remains provisional.
**Closed since first writing:**

4. ~~Licensing (carried from A2 §11)~~ — **settled 2026-08-19.** The user holds a
   BLPAPI licence permitting programmatic Desktop API use, with a daily allowance
   of **500,000 hits**. Two consequences. The design constraint "call Bloomberg
   only when needed" stands on its own merits and is unchanged — a resolved
   instrument is never re-resolved, and typing never calls out. But the *margin*
   is far wider than the estimator was tuned for: the A2 hit accounting was
   calibrated against the Excel add-in and treats the budget as scarce. Nothing
   in P1 depends on that calibration, so it is left alone here; the soft limit is
   a user setting and can be raised whenever the estimator is revisited.

---

## 10. Second pass — closing the open items (2026-08-19)

The four items §9 left open have been resolved. Three were settled against the
live Terminal in one session; the fourth was answered by the user. Evidence:
`blpapi-facts/p0_close_report.json`, reproduced by
`blpapi-facts/blp_p0_close_probe.py`.

Sixteen mnemonics were put to `FieldInfoRequest` and **all sixteen exist**,
including `CA_MA_COMPLETE_DT`, which the earlier partial field dump did not
contain. Absence from that dump was not evidence: it holds 8,637 mnemonics,
far short of the dictionary.

### 10.1 Adjustment factor operator and flag semantics — SETTLED

Bloomberg documents both columns on the field itself. `EQY_DVD_ADJUST_FACT`
returns four unlabelled columns:

| Column | Meaning |
|---|---|
| 1 | Adjustment Date |
| 2 | Adjustment Factor |
| 3 | **Operator Type — 1 = divide, 2 = multiply, 3 = add. *Opposite for Volume.*** |
| 4 | **Flag — 1 = prices only, 3 = prices and volumes** |

Measured on AAPL, default (no override), five rows, one per split:

| Adjustment Date | Factor | Operator | Flag |
|---|---|---|---|
| 2020-08-31 | 4.0 | 1 | 3 |
| 2014-06-09 | 7.0 | 1 | 3 |
| 2005-02-28 | 2.0 | 1 | 3 |
| 2000-06-21 | 2.0 | 1 | 3 |
| 1987-06-16 | 2.0 | 1 | 3 |

Cumulative split factor 4 × 7 × 2 × 2 × 2 = **224**.

**This cross-checks §3.1 exactly.** That section measured AAPL's 2020-08-28
close at 499.23 raw and 124.81 split-adjusted. Operator 1 means divide:
499.23 / 4 = 124.8075. Had operator 1 meant multiply, the answer would have been
1996.92. The factor table and the price measurement were taken independently,
days apart, and they agree — which is the strongest evidence in this document
that the adjustment model is understood correctly.

**The trap is "Opposite for Volume".** The same operator code means divide for a
price and multiply for a volume. A derivation engine that applies one rule to
both columns produces volumes wrong by the square of the factor — for AAPL,
50,176× — while prices look perfectly right. P4 must carry the price/volume
distinction into the factor application itself, not into a wrapper around it.

**Flag 1 means prices only.** With `CORPORATE_ACTIONS_FILTER` set to
`NORMAL_CASH|ABNORMAL_CASH|CAPITAL_CHANGE`, AAPL returns 97 rows: the same 5
splits at operator 1 / flag 3, plus 92 cash dividends at **operator 2 (multiply),
flag 1 (prices only)**, factors just below 1 (0.999138 on 2026-08-10). Dividends
correctly leave volume untouched. The five splits survive the filter, so the
filtered call is a superset — one request, not two.

### 10.2 `SIMP_SEC_STATUS` — SETTLED, and it is not what its name suggests

**It is a realtime trading-session status, not a security lifecycle status.**
Bloomberg's own documentation: "Bloomberg normalized (simplified) representation
of the **trading status** of a security, taking into account the Trading Period
and Exchange Market Status values. **Field updates in realtime.**"

The value domain, quoted from the field documentation: `ABAL` auction order book
balancing, `AUCT` auction call phase, `CAUC` continuous auction pre-call,
`CLOS` market closed, `HALT` temporary halt, `HLTA` halt and auction call,
`INEG` ineligible to trade (pending listing, delisted, private company, expired,
called), `MOC` / `MOCB` market-on-close order collection and balancing,
`NONE` undefined, `NOTR` not present, `PREO` pre-open, and others.

Observed across a mixed basket at 15:26 local time:

| Security | Value |
|---|---|
| AAPL US Equity | `PREO` |
| META US Equity | `PREO` |
| VFIAX US Equity | `CLOS` |
| FB US Equity | `PREO` |
| TWTR US Equity | `HALT` |
| SX5E Index | *(no value returned)* |

**Three consequences, all of which change the design.**

First, it must not be stored as an instrument attribute. Recording that AAPL's
status has been `PREO` since its 1980 listing is not a fact about the
instrument; it is a fact about the clock, and it would be wrong again within
the hour.

Second, it is an intraday field on a system that has deliberately restricted
itself to end-of-day data. Fetching it spends a call on a value that is stale on
arrival.

Third, absence proves nothing. The field "will only return a value through
BLPAPI for securities that employ the latest version of the security status
(version 2)"; `SECURITY_STATUS_VERSION` says which. `SX5E Index` returned
nothing, and that is not evidence about the index.

**Resolution: drop `SIMP_SEC_STATUS` from the identity block entirely.** The
lifecycle question it was recruited to answer — is this instrument still alive —
is already answered by `INACTIVE_DATE`, which is dated, permanent, and already
in the identity field set. Open question §9.3 is closed by removing the need
for it rather than by satisfying it.

`INEG` is the one value carrying lifecycle meaning, but it is a live state, not
a dated event, and it collapses "pending listing", "delisted", "private" and
"expired" into one code. `INACTIVE_DATE` is strictly better for every use P1
through P5 has.

### 10.3 The `CA_MA_*` family — SETTLED, 89 members

A live `FieldSearchRequest` for `CA_MA` returns 89 fields. The full list is in
the capture; the ones that matter for fund mergers and successor derivation:

| Mnemonic | Type | Meaning |
|---|---|---|
| `CA_MA_COMPLETE_DT` | Date | Completion/Termination Date. "A publicly traded Target's completion date coincides with the delisting date. Termination date will indicate date which the deal was officially withdrawn." |
| `CA_MA_ACQUIRER_TICKER` | LongCharacter | Acquirer Ticker |
| `CA_MA_ACQUIRER_NAME` | LongCharacter | Acquirer Name |
| `CA_MA_TARGET_TICKER` | LongCharacter | Target Ticker |
| `CA_MA_TARGET_NAME` | LongCharacter | Target Name |
| `CA_MA_DEAL_TYPE` | Character | Deal Type |
| `CA_MA_AMENDMENT_DT` | Date | Amendment Date |
| `CA_MA_EXPECTED_COMPLETION_DATE` | Date | Expected Completion Date |
| `CA_MA_DROP_DEAD_DATE` | Date | Drop Dead Date |
| `CA_MA_PCT_SOUGHT` / `CA_MA_PCT_OWNED` | Real | Percent sought / owned |
| `CA_MA_STOCK_TERMS` / `CA_MA_CASH_TERMS` | Character | Consideration terms |

Note `CA_MA_COMPLETE_DT` carries two distinct meanings in one column —
completion and termination — distinguished only by context. A withdrawn deal and
a consummated one both stamp a date here. P3 must not read a non-null value as
"this merger happened".

### 10.4 Correction to §7.2

§7.2 states flatly that no Bloomberg field returns a successor security. **That
is too strong, and this pass corrects it.**

It holds for *renames and lifecycle*: nothing maps a delisted instrument to
whatever replaced it, which is why `HISTORICAL_IDS_TIME_RANGE` plus human
confirmation remains the mechanism for those.

It does **not** hold for *mergers and acquisitions*. `CA_MA_ACQUIRER_TICKER` and
`CA_MA_ACQUIRER_NAME` name the acquiring entity directly, and
`CA_MA_COMPLETE_DT` dates it. Three of the six mnemonics §7.1 recorded as
non-existent — `MERGER_ACQUIRER`, `EQY_ACQUIRER_NAME`, `MERGER_TARGET` — were
not absent concepts, only wrong names for concepts that exist under the `CA_MA_`
prefix. §7.1 remains correct as written (those six strings do not exist); what
it must not be read to imply is that the information is unavailable.

The design consequence is unchanged for P1: an M&A ticker is still a *proposal*,
because the acquirer's ticker is a string that must itself be resolved, and
because a withdrawn deal populates the same fields. But P5 has a real source
rather than pure inference, which is a materially better starting point than
§7.2 implied.

### 10.5 What remains open

One item, and it is not answerable by probing:

- **Is `instrumentListRequest` metered, and at what rate?** Not exposed by the
  API, and not visible in any response. It stays counted conservatively. With
  the licence now known to allow 500,000 hits/day (§9.4), the practical risk of
  the conservative estimate being wrong in either direction is small.

`//blp/instruments` remains the only search surface, and no corporate-actions
service exists (§2.1) — both re-confirmed by this session's service list.
