# P6: automatic merger lifecycle — design

Date: 2026-08-20. Status: SHIPPED (same day), live-verified on YODA LN
(status ACQU recorded, lifecycle_dead_fund issue raised, 3 hits charged,
cooldown then held at zero cost).
Evidence: live probes 2026-08-20 (`src-tauri/scripts/probe_merger.py`,
`probe_fields.py`; raw captures in the session scratchpad). Every field named
below was measured against this user's Desktop API licence — nothing is
assumed from documentation or support mail.

## 1. What the user asked for

Fund mergers handled automatically, with no human interaction where the data
allows it — in particular the exchange ratio of merged funds — while keeping
daily Bloomberg hit consumption minimal (the 500k/day limit must stay
available for wide daily EOD runs, not lifecycle plumbing).

## 2. What the probes established

### Works with this licence

| Fact | Evidence |
|---|---|
| `MARKET_STATUS` (1 hit, scalar, works on funds AND equities) returns `ACTV` for a live security and `ACQU` for an acquired one | SCHDYXA=ACTV; OMUSEAA (Merian, absorbed by Jupiter 2020)=ACQU; **YODA (book instrument 2)=ACQU** |
| `HISTORICAL_IDS_TIME_RANGE` works on exchange-listed funds; delist rows carry Bloomberg's own `Action ID` per line | YODA LN delisted 2024-04-19, Action ID 238004028 |
| `"<ActionID> Action"` is a valid ReferenceDataRequest security string | resolves with no securityError |
| The `CA_MA_*` family is entitled and populated on M&A deal actions | 222633226 (XLNX/AMD), 184588814 (CELG/BMY) |
| `CA_MA_STOCK_TERMS` carries the exchange ratio in a stable format: `"1.7234 Aqr sh./Tgt sh."` — r = acquirer shares per target share | both deals; direction cross-checked against raw price continuity (194.92/114.27 = 1.706 ≈ 1.7234) |
| `MERGERS_AND_ACQUISITIONS` (bulk, equities only) lists every deal with `Action Id`, `Deal Type` (M&A/INV), `Deal Status`, `Announcement Date` | XLNX: 18 rows; CELG: BMY deal 184588814 |

### Does not work — designed around, not retried

- `TARGET_SHARES_RATIO` and `ACTION_TYPE`: `NOT_ENTITLED` everywhere.
- `MERGERS_AND_ACQUISITIONS`, `EQY_DVD_ADJUST_FACT`, `INACTIVE_DATE`,
  `HISTORICAL_IDS_TIME_RANGE` (unlisted classes), `CA_MA_*`-on-the-security:
  *not applicable to funds* (share-class securities).
- Widened `CORPORATE_ACTIONS_FILTER` tokens are silently ignored; the DVD
  tables never contain merger rows.
- Delisting Actions answer no `CA_MA_*` fields (only deal actions do).
- No field points from a dead fund to its absorber. For a true fund
  absorption the API names no successor — the Terminal (CACX) does. Full
  automation is therefore possible for equity stock deals and for ticker
  chains; a fund absorption ends as a *rich proposal*, one click from done.
- `CA_MA_PAYMENT_TYP` is **localized** ("Cash et Actions" on this French
  Terminal). Never parse it; the presence of STOCK_TERMS/CASH_TERMS is the
  reliable signal.

## 3. Design

### 3.1 Detection — event-driven, near-zero standing cost

New module `lifecycle.rs`. After every completed run/backfill (advisory
post-run hook, live paths only, like `corp_actions_after`):

1. **Candidates** (pure SQL, 0 hits): active book instruments in active
   views that hold a current security but have no current observation dated
   within the last 7 calendar days, and no status recorded in the last 30
   days (cooldown). Deliberately including members with zero observations
   ever — a security already dead when added (live case: YODA) would
   otherwise burn hits daily while staying invisible. On a healthy book
   this set is empty and the hook costs nothing.
2. **Status check**: one batched `MARKET_STATUS` request (1 hit/instrument).
   The verbatim value is recorded under the bitemporal `status` attr — the
   slot migration 0001 reserved for "a lifecycle status P3/P5 may derive"
   (source `lifecycle`). `ACTV` → stop (the gap is something else; the
   cooldown stops re-asking for 30 days).
3. **Non-ACTV → equity route**: `MERGERS_AND_ACQUISITIONS` (1 hit).
   Completed M&A deals, newest first, capped at 3: fetch
   `"<ActionID> Action"` terms (6 hits each) until
   `CA_MA_TARGET_TICKER` matches this instrument's own ticker+exchange —
   the deal list also contains deals where the instrument was the
   *acquirer*, so the target check is the discriminator, not recency.
4. **Match**: propose `merger` link (predecessor = us, successor = the
   instrument currently wearing `CA_MA_ACQUIRER_TICKER + " Equity"`),
   effective = `CA_MA_COMPLETE_DT`, evidence = the verbatim terms + deal
   row. If `CA_MA_STOCK_TERMS` parses and the acquirer resolves to exactly
   one local instrument → **auto-confirm** as `auto:action:<id>` and store
   the ratio. Anything less certain (cash-only deal, unparsed terms,
   acquirer unknown or ambiguous locally) stays an unconfirmed proposal
   plus an `ingest_issue` — the P0 7.2 confirmation gate is relaxed only
   where Bloomberg *asserts* the link, never where it is inferred.
5. **Fund route** (`MERGERS_AND_ACQUISITIONS` not applicable): identifier
   history anchored on the instrument's own security (1 hit) — this
   ingests delist Action IDs into `instrument_alias.bbg_action_id` and, if
   Bloomberg recorded a ticker chain, the existing history machinery
   proposes the `rename` on its own. If no successor emerges, a durable
   `ingest_issue` (`lifecycle_dead_fund`) records status + Action IDs.
   Once the human confirms a successor, stitching derives the junction
   ratio from NAV continuity at the effective date — which for fund
   mergers is not an approximation but the official mechanism (support
   mail: ratios "are often determined from NAVs on the effective date").

The auto-confirm justification, stated once: today's links are *inferred*
from ambiguous identifier evidence and rightly gated. A CA record with an
Action ID naming target, acquirer, completion date and terms is *asserted by
Bloomberg*. Asserted links auto-confirm; inferred links still queue.

### 3.2 Terms storage — migration 0006

```sql
ALTER TABLE instrument_link
  ADD COLUMN exchange_ratio DOUBLE PRECISION CHECK (exchange_ratio > 0),
  ADD COLUMN terms JSONB;
```

`exchange_ratio` is Bloomberg's r (acquirer shares per target share), parsed
from `CA_MA_STOCK_TERMS`. `terms` is the verbatim CA_MA payload — same
authority-vs-extraction split as `corp_action.payload`.

### 3.3 Stitching uses the asserted ratio

`stitch.rs` junction ratio precedence for `merger`/`conversion`:
1. `exchange_ratio` present → multiplier = **1/r** (1 target share becomes r
   acquirer shares, so target prices divide by r to land in successor
   units; pinned by the XLNX 194.92 / 1.7234 ≈ AMD 114.27 measurement).
2. Otherwise → price-continuity derivation, exactly as today.

`SegmentInfo.note` says which source produced the multiplier.

### 3.4 Budget

Charged at the wire seam like everything else. Purposes: `lifecycle`
(status, deal list, history), `merger_terms` (Action queries). Worst case
per dead instrument: 1 + 1 + 3×6 = 20 hits, once — then the link exists,
the cooldown holds, and (bonus) a linked-dead instrument stops burning
2 field-hits/day in the daily run once the user retires it, which the
issue tells them to do. The standing cost of the feature on a healthy book
is zero.

## 4. Out of scope

- Enumerating a fund's corp actions to find an absorption Action ID: no
  entitled request exposes it; guessing nearby Action IDs is fabrication.
- Parsing `CA_MA_CASH_TERMS` into money: stored verbatim in `terms`,
  surfaced in evidence, not modelled (a cash-out has no series to stitch).
- `SIMP_SEC_STATUS` stays excluded; `MARKET_STATUS` is a lifecycle answer,
  not a session mood, and is stored bitemporally like any other attr.
