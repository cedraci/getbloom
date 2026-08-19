# P1 — Instrument/Security Master (design)

**Date:** 2026-08-19
**Status:** PROPOSED — awaiting review
**Branch:** `bloomberg-security-master`
**Depends on:** `2026-08-19-blpapi-field-facts.md` (P0). Every Bloomberg field
name used here is verified there. **Do not add a mnemonic to this design that
P0 has not confirmed.**
**Supersedes:** the `asset` table of `2026-08-13-bloomberg-eod-pipeline-design.md` §3.
**Preserves:** the A2 fetch/ingest architecture, the scheduler, and the hit budget.

---

## 1. Why

The current model makes the ticker the identity. `asset` carries
`UNIQUE (bdp_security)`, and `bdp_security` is derived from a mutable ticker.
When a ticker changes, the only options are to edit the row — destroying the
past — or add a second row, splitting one instrument's history in two.

P0 §6.4 demonstrated the cost concretely. `META US Equity` and the Roundhill
Ball Metaverse ETF have both worn the ticker `META`. A ticker is not an
identity; it is an attribute that an instrument holds for a period.

This phase introduces a durable internal identity, records identifiers as
validity periods, and resolves user input to it through Bloomberg with an
audit trail.

---

## 2. Scope

**In scope**

- The `instrument` identity spine and its attribute, alias and link tables.
- Resolution of ticker/ISIN input to an instrument, with a manual-review queue.
- Two-tier search: local fuzzy matching, and an explicit Bloomberg lookup.
- The complete schema skeleton, including the new `observation` shape.
- Retargeting `view`, `run`, `ingest_issue` and the Excel import/export from
  `asset_id` to `instrument_id`.

**Out of scope** — deliberately, with the phase that owns each

- Writing or reading observations (**P2**). P1 creates the table; it stays empty.
- Corporate-action ingestion (**P3**), the adjustment engine (**P4**),
  fund mergers and holdings transformation (**P5**).
- The persistent BLPAPI session layer. Considered and **dropped from P1**: it
  was only needed for per-keystroke search, and §6 does not call Bloomberg
  while typing. The existing one-shot sidecar is adequate for a deliberate
  button press (P0 measured 0.8 s for session start plus a live request).

**Non-goal:** minimising the number of Bloomberg requests is a hard constraint,
not an optimisation. See §7.

---

## 3. Prerequisite — the database is rebuilt, not migrated

The existing database is disposable (user, 2026-08-19), and its observations
have negative value: P0 §3.2 established that every row was fetched with none
of the four adjustment flags set, making it neither raw nor reproducible.

Migrations `0001`–`0004` are therefore **consolidated into a single new
`0001_init.sql`** describing the schema below. No data migration is written.

**This makes a reset mandatory, not optional.** sqlx records each migration's
checksum in `_sqlx_migrations`; a database that already ran the old `0001` will
fail its checksum at startup, and migrations run at startup, so the app will not
boot. Required steps, in order:

1. Export the asset book via the existing Excel export (it is the migration tool).
2. `dropdb bloomdata && createdb bloomdata` (and likewise `bloom_test`).
3. Start the app; the new `0001` applies to an empty database.
4. Re-import the book. Each row resolves through §5, which is also the first
   real exercise of the resolution path.

The reasoning behind the retired migrations survives in git history. Migration
`0004`'s actual protection — the doubled-yellow-key repair — lives in
`registry::resolve_bdp_security` with its regression test, not in SQL, so
nothing is lost by dropping it.

---

## 4. Data model

### 4.1 `instrument` — the immutable spine

```sql
CREATE TABLE instrument (
  instrument_id  BIGSERIAL PRIMARY KEY,
  id_bb_global   TEXT UNIQUE,          -- FIGI; null until resolved
  id_bb_unique   TEXT,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
```

Nothing else. No ticker, no ISIN, no name, no status — all of those change.
**No statement anywhere in the codebase may `UPDATE` this table** except to fill
`id_bb_global` / `id_bb_unique` when they move from null to known, which is a
one-way transition enforced by a trigger.

`id_bb_global` is nullable because an instrument may be created by a user before
Bloomberg has been asked about it.

### 4.2 `instrument_attr` — bitemporal attributes

```sql
CREATE TABLE instrument_attr (
  id             BIGSERIAL PRIMARY KEY,
  instrument_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  attr           TEXT NOT NULL,        -- 'name' | 'exchange' | 'country' |
                                       -- 'currency' | 'asset_class' |
                                       -- 'instrument_type' | 'issuer' |
                                       -- 'share_class' | 'fund_vehicle' |
                                       -- 'status'
  value          TEXT NOT NULL,
  valid_from     DATE NOT NULL,
  valid_to       DATE NOT NULL DEFAULT 'infinity',
  system_from    TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to      TIMESTAMPTZ NOT NULL DEFAULT 'infinity',
  source         TEXT NOT NULL,        -- 'bloomberg' | 'user' | 'derived'
  decision_id    BIGINT REFERENCES resolution_decision(id)
);
CREATE UNIQUE INDEX instrument_attr_current
  ON instrument_attr (instrument_id, attr, valid_from)
  WHERE system_to = 'infinity';
```

Two time axes from the start. `valid_from`/`valid_to` say when the fact was true
in the world; `system_from`/`system_to` say when we believed it. A correction
closes the old row's `system_to` and inserts a new one — it never overwrites.
This is what makes both readings the objectives require possible:

- *best currently known history* — filter `system_to = 'infinity'`
- *point-in-time* — filter `system_from <= T AND system_to > T`

Retrofitting a system-time axis later is far harder than carrying it now, which
is why it appears in P1 even though P2 is what exploits it.

Attribute values are sourced from the P0-verified fields: `NAME`, `EXCH_CODE`,
`CNTRY_ISSUE_ISO`, `CRNCY`, `SECURITY_TYP2`, `MARKET_SECTOR_DES`,
`ID_BB_COMPANY`, `FUND_SHR_CLASS_DESG`, `SHARE_CLASS_TYPE`, `FUND_TYP`.
Validity dates come from `LISTING_DATE` and `INACTIVE_DATE`.

The `status` attribute is defined in the CHECK constraint but **nothing in P1
writes it.** `SIMP_SEC_STATUS` was the intended source, and P0 §10.2 established
it is a realtime *trading-session* status — `PREO`, `CLOS`, `HALT` — not a
lifecycle status. Storing it as an attribute would record the market clock as a
property of the instrument. Lifecycle comes from `INACTIVE_DATE`, which is dated
and permanent. The attribute value stays in the domain for P3/P5 to derive.

### 4.3 `instrument_alias` — every identifier ever worn

```sql
CREATE TABLE instrument_alias (
  id                   BIGSERIAL PRIMARY KEY,
  instrument_id        BIGINT NOT NULL REFERENCES instrument(instrument_id),
  id_type              TEXT NOT NULL CHECK (id_type IN
                         ('ticker','isin','figi','cusip','sedol','bbg_unique',
                          'bdp_security')),
  value                TEXT NOT NULL,
  exch_code            TEXT,
  valid_from           DATE NOT NULL,
  valid_to             DATE NOT NULL DEFAULT 'infinity',
  system_from          TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to            TIMESTAMPTZ NOT NULL DEFAULT 'infinity',
  source               TEXT NOT NULL,   -- 'bloomberg_hist_ids' | 'bloomberg_ref'
                                        -- | 'user'
  bbg_action_id        TEXT,            -- 'Action ID' from HISTORICAL_IDS_TIME_RANGE
  anchoring_identifier TEXT             -- REQUIRED when source =
                                        -- 'bloomberg_hist_ids'
);
CREATE INDEX ON instrument_alias (id_type, lower(value));
ALTER TABLE instrument_alias ADD CONSTRAINT alias_anchor_required
  CHECK (source <> 'bloomberg_hist_ids' OR anchoring_identifier IS NOT NULL);
```

`anchoring_identifier` is not bookkeeping. P0 §6.4 showed that
`HISTORICAL_IDS_TIME_RANGE` asked about `META US Equity` returns Facebook's
rename *or* the Roundhill ETF's rename depending on whether
`HISTORICAL_STARTING_IDENTIFIER` was supplied. An alias row whose anchor is
unknown cannot be trusted, so the CHECK constraint makes it impossible to store
one.

A ticker change closes the old row and inserts a new one. **No `UPDATE` ever
touches `value`.**

`bbg_action_id` is Bloomberg's own event id (e.g. `228233742`) and is the key on
which an amended or withdrawn identifier change is recognised in P3.

### 4.4 `instrument_link` — successor and predecessor

```sql
CREATE TABLE instrument_link (
  id              BIGSERIAL PRIMARY KEY,
  predecessor_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  successor_id    BIGINT NOT NULL REFERENCES instrument(instrument_id),
  link_type       TEXT NOT NULL CHECK (link_type IN
                    ('rename','merger','conversion','share_class_change','spinoff')),
  effective_date  DATE NOT NULL,
  evidence        JSONB NOT NULL,      -- the Bloomberg rows that suggested it
  confirmed_by    TEXT,                -- null = proposed, not active
  confirmed_at    TIMESTAMPTZ,
  CHECK (predecessor_id <> successor_id)
);
```

P0 §7.2 stated that no Bloomberg field returns a successor security. P0 §10.4
**narrows that**: it holds for renames and lifecycle, but not for M&A, where
`CA_MA_ACQUIRER_TICKER` and `CA_MA_ACQUIRER_NAME` name the acquirer directly and
`CA_MA_COMPLETE_DT` dates it. All four mnemonics are confirmed to exist
(P0 §10.3, 89 members in the family).

Links remain derived and remain proposals regardless — from
`HISTORICAL_IDS_TIME_RANGE`, `CA_MA_COMPLETE_DT`,
`REVERSE_MERGER_COMPLETION_DATE`, `FUND_SHARE_CLASS_CLOSURE_DATE` — for two
reasons that an acquirer ticker does not remove. An acquirer *ticker* is a
string that must itself be resolved to an instrument, and P0 §6.4 is the standing
demonstration that a ticker is not an identity. And `CA_MA_COMPLETE_DT` is
documented as *"Completion/Termination Date"*: a withdrawn deal stamps the same
column as a consummated one, so a non-null value is not evidence the merger
happened.

A link with `confirmed_by IS NULL` is therefore a proposal that no query may
follow. That nullability is the integrity guarantee, not a convenience.

### 4.5 `resolution_decision` and `resolution_review`

```sql
CREATE TABLE resolution_decision (
  id              BIGSERIAL PRIMARY KEY,
  raw_input       TEXT NOT NULL,
  normalized      TEXT NOT NULL,
  hint_exchange   TEXT,
  hint_country    TEXT,
  hint_currency   TEXT,
  hint_asset_class TEXT,
  method          TEXT NOT NULL CHECK (method IN
                    ('local_alias','bloomberg_ref','bloomberg_list','manual')),
  chosen_instrument_id BIGINT REFERENCES instrument(instrument_id),
  candidates      JSONB NOT NULL,      -- every candidate considered, with scores
  bbg_response    JSONB,               -- the response, unedited
  decided_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  decided_by      TEXT NOT NULL        -- 'auto' or a user
);

CREATE TABLE resolution_review (
  id            BIGSERIAL PRIMARY KEY,
  decision_id   BIGINT NOT NULL REFERENCES resolution_decision(id),
  status        TEXT NOT NULL CHECK (status IN ('pending','resolved','rejected')),
  opened_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  closed_at     TIMESTAMPTZ,
  note          TEXT NOT NULL DEFAULT ''
);
```

Storing `bbg_response` unedited preserves the A2 audit-trail property: what
Bloomberg actually said is recoverable, not just what was concluded from it.

### 4.6 `book_entry` — the user's book, replacing `asset`

```sql
CREATE TABLE book_entry (
  instrument_id  BIGINT PRIMARY KEY REFERENCES instrument(instrument_id),
  label          TEXT NOT NULL,
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  added_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  note           TEXT NOT NULL DEFAULT ''
);
```

`asset` disappears. Identity belongs to `instrument`; the user's own label and
active flag belong here. There is no `UNIQUE (bdp_security)` — one instrument
legitimately wears several security strings over time, so that constraint
becomes wrong rather than merely unnecessary. Uniqueness now falls where it
belongs: one book entry per instrument.

The current security string is derived from the alias valid today
(`id_type = 'bdp_security'`), not stored on the entry.

### 4.7 `instrument_candidate` — the local search corpus

```sql
CREATE TABLE instrument_candidate (
  id             BIGSERIAL PRIMARY KEY,
  security       TEXT NOT NULL UNIQUE,  -- normalised: 'AAPL US Equity'
  raw_security   TEXT NOT NULL,         -- as returned: 'AAPL US<equity>'
  description    TEXT NOT NULL,
  yellow_key     TEXT,
  first_seen     TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen      TIMESTAMPTZ NOT NULL DEFAULT now(),
  instrument_id  BIGINT REFERENCES instrument(instrument_id)  -- once resolved
);
```

Every row Bloomberg has ever returned from `instrumentListRequest` is kept
forever. This is what makes §6 free: one search for "AAPL" seeds all thirteen
Apple listings permanently.

### 4.8 `observation` — created here, written in P2

```sql
CREATE TABLE adjustment_basis (
  id            SMALLSERIAL PRIMARY KEY,
  adj_normal    BOOLEAN,
  adj_abnormal  BOOLEAN,
  adj_split     BOOLEAN,
  adj_follow_dpdf BOOLEAN,
  note          TEXT NOT NULL DEFAULT ''
);
-- seeded with: RAW (all four false), and LEGACY_DPDF (all null, note explains
-- that the flags were unset and the Terminal's DPDF setting was not captured).

CREATE TABLE observation (
  id             BIGSERIAL PRIMARY KEY,
  instrument_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  field_id       BIGINT NOT NULL REFERENCES field_def(id),
  obs_date       DATE NOT NULL,
  obs_time       TIME,                        -- null for EOD
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
  CHECK ((value_num IS NULL) <> (value_text IS NULL)),
  CHECK ((granularity = 'eod') = (obs_time IS NULL))
);
CREATE UNIQUE INDEX observation_current ON observation
  (instrument_id, field_id, obs_date, obs_time, granularity, layer, basis_id)
  WHERE system_to = 'infinity';
```

Four decisions worth stating explicitly:

**Raw is never overwritten.** There is no `ON CONFLICT DO UPDATE` anywhere. A
correction inserts a new row and closes the previous one's `system_to`. The
partial unique index enforces one current row per logical series while allowing
the full superseded history to accumulate beneath it.

**The five observation classes are a column, not five tables.** `layer`
distinguishes raw, Bloomberg-adjusted, internally derived, total-return and
holdings-transformed values of the same instrument-day, so they can be compared
in one query and can never be silently mixed.

**Adjustment basis is recorded, not assumed.** `basis_id` names the exact flag
combination that produced the value (P0 §3). This is the user's 2026-08-19
decision: granular truth in preference to an "unknown basis" marker.

**The time axis admits intraday without a rewrite.** `obs_time` and
`granularity` are present and constrained so that EOD rows are unambiguous
(`obs_time IS NULL`) while an intraday granularity can be added later as new
values, not a schema change. Intraday is explicitly not built now.

### 4.9 Retargeted and retained

`asset_class`, `field_def`, `view`, `view_field`, `run`, `ingest_issue`,
`hit_ledger` and `schedule` all survive. `view_asset` becomes
`view_instrument (view_id, instrument_id)`, and `ingest_issue.asset_id` becomes
`instrument_id`.

`field_def` gains three columns to become the configurable field-mapping layer
the objectives require: `bbg_ftype` (P0 §5 — `BulkFormat` marks a table-valued
field), `bbg_datatype`, and `entitlement_note`. Adding a field remains an
INSERT, never a migration.

---

## 5. Resolution

```
input (ticker and/or ISIN, optional hints)
  │
  ├─1  normalise: trim, upper-case, strip a trailing yellow key
  │       (reuse registry::resolve_bdp_security's proven logic)
  ├─2  local alias lookup, as of the relevant date
  │       hit → done, method 'local_alias', no Bloomberg call
  ├─3  ReferenceDataRequest for the identity block
  │       ID_BB_GLOBAL, ID_BB_GLOBAL_SHARE_CLASS_LEVEL, ID_ISIN, EXCH_CODE,
  │       CRNCY, CNTRY_ISSUE_ISO, SECURITY_TYP2, MARKET_SECTOR_DES,
  │       LISTING_DATE, INACTIVE_DATE
  │       unambiguous → bind, method 'bloomberg_ref'
  ├─4  ambiguous → instrumentListRequest with the matching yellowKeyFilter
  ├─5  score candidates on exchange, country, currency, asset class
  │       exactly one survivor → bind, method 'bloomberg_list'
  └─6  two or more survivors → resolution_review row, status 'pending',
         NOTHING IS BOUND
```

Every path writes a `resolution_decision` carrying all candidates, their scores
and the unedited Bloomberg response — including path 2, which records that no
call was made.

**Scoring** is a deterministic, additive rule over the supplied hints, not a
learned model: an exact match on a supplied hint adds to the score, a
contradiction disqualifies the candidate outright. A candidate missing a hint
neither gains nor loses. If the top two scores tie, the result is ambiguous by
definition and goes to review.

**Option contracts are excluded** from candidate sets. P0 §6 observed that a
query for `AAPL` returns `AAPL US 08/21/26 C400<equity>` alongside the listings;
these are filtered on the derivative-shaped security pattern before scoring.

**Nothing binds silently.** A `pending` review blocks the instrument from
entering any view, so an unresolved identifier cannot quietly become a gap in a
time series.

### 5.1 Identifier history

On first resolution, one `HISTORICAL_IDS_TIME_RANGE` request is issued with
`HISTORICAL_STARTING_IDENTIFIER` set to the resolved security and
`HISTORICAL_ID_TM_RANGE_START_DT` set to `LISTING_DATE` (or a configured floor).
Returned rows become `instrument_alias` rows with `valid_from`/`valid_to` from
the `Date` column, `bbg_action_id` from `Action ID`, and `anchoring_identifier`
set to the identifier that was passed.

An `Old ID` that already belongs to a different instrument is **not** merged
automatically: it opens an `instrument_link` proposal with `confirmed_by NULL`.
This is the FB/META/METV case, and it is exactly where an automatic merge would
destroy history.

---

## 6. Search

### 6.1 Local tier — every keystroke, zero Bloomberg calls

`CREATE EXTENSION pg_trgm;` (verified present: PostgreSQL 17 ships pg_trgm 1.5
on this machine, unlike TimescaleDB).

A GIN trigram index covers a `search_text` built from: `book_entry.label`,
every current and historical `instrument_alias.value`, the `name` attribute,
and `instrument_candidate.security` + `description`. Ranked by
`similarity()`, filtered by a minimum threshold.

Results are labelled by origin — **in your book**, **known instrument**, or
**seen before** (candidate cache) — so the user can tell an existing instrument
from a new one before clicking.

The corpus grows monotonically with use: every Bloomberg search and every
resolution enriches it permanently, and none of that growth costs a second call.

### 6.2 Bloomberg tier — explicit, never automatic

A **Search Bloomberg** button, surfaced when local results are thin. It shows
the estimated hit cost before the call and records the actual call in
`hit_ledger` after. It is never triggered by typing, focus, or navigation.

Results are normalised — `AAPL US<equity>` → `AAPL US Equity` — and stored in
`instrument_candidate`. **The raw form must never be used as a security
string**; pasting it would produce exactly the malformed identifier that
migration `0004` had to repair.

Selecting a suggestion runs the full §5 resolution. It does not bind the clicked
string directly; the type-ahead feeds the security master rather than bypassing
it.

---

## 7. Hit budget

The constraint is hard: the tool calls Bloomberg only when it must.

| action | Bloomberg calls |
|---|---|
| typing in the search box | **none, ever** |
| selecting a locally-known instrument | none |
| explicit "Search Bloomberg" | 1 `instrumentListRequest` |
| resolving a never-seen instrument | 1 `ReferenceDataRequest` + 1 `HISTORICAL_IDS_TIME_RANGE`, once per instrument for its lifetime |
| re-resolving a known instrument | none — served from `instrument_alias` |

All of these are recorded in `hit_ledger` and counted conservatively, matching
the existing over-count-is-safe policy. Whether `instrumentListRequest` is
metered at all is unknown (§10).

---

## 8. UI

- **Assets screen** becomes the book: search box (§6.1), a **Search Bloomberg**
  button, and the book list with each entry's current security string, status
  and resolution state.
- **Review queue** — a new screen listing `resolution_review` rows: the input,
  the candidates with scores, and the Bloomberg response, with actions to choose
  a candidate, reject, or defer. Also lists unconfirmed `instrument_link`
  proposals.
- **Instrument detail** — attribute history and alias history as timelines, so
  a ticker change reads as two validity periods rather than an edit.
- **Excel import/export** is retargeted to `book_entry` + resolution. An
  imported row that resolves ambiguously creates a review row instead of
  failing, and the id-column guardrail from the 2026-08-18 work is preserved.

---

## 9. Testing

- **Pure unit tests, no Bloomberg:** normalisation, candidate scoring including
  the tie case, option-contract filtering, `AAPL US<equity>` → `AAPL US Equity`,
  and bitemporal close-and-insert on both `instrument_attr` and
  `instrument_alias`.
- **Fixture tests:** the P0 captures in `docs/superpowers/specs/blpapi-facts/`
  are checked in and replayed — in particular the FB/META and META/METV rows,
  which become the regression test for §5.1's refusal to auto-merge.
- **Integration (Postgres only):** resolution end to end against a mock fetcher;
  a ticker change producing two alias rows and zero updates; `pg_trgm` search
  ranking.
- **Live smoke (Bloomberg machine):** resolve one equity by ticker, one by ISIN,
  and one fund share class; confirm the review queue opens for a deliberately
  ambiguous input.
- **An assertion worth writing as a test:** no code path issues an `UPDATE`
  against `instrument`, or against `value` on `instrument_alias`.

---

## 10. Open questions

1. ~~**`Adjustment Factor Operator Type` and `Adjustment Factor Flag`
   semantics**~~ — **settled 2026-08-19** (P0 §10.1). Operator 1 = divide,
   2 = multiply, 3 = add, **opposite for volume**; flag 1 = prices only,
   3 = prices and volumes. Verified against AAPL's five splits, and it
   reproduces P0 §3.1's measured 499.23 / 4 = 124.81 exactly. P4 must carry the
   price/volume distinction into factor application: the same operator code
   means divide for a price and multiply for a volume.
2. **Is `instrumentListRequest` metered?** Not public, not established by P0.
   Counted conservatively until observed.
3. ~~**`SIMP_SEC_STATUS` value domain**~~ — **closed 2026-08-19 by removing the
   need for it** (P0 §10.2). It is a realtime trading-session status, not a
   lifecycle status, so it is dropped from the identity block entirely.
   `INACTIVE_DATE` answers the lifecycle question, dated and permanently.
4. ~~**Licensing**~~ — **settled 2026-08-19**: the user holds a BLPAPI licence for
   programmatic Desktop API use, 500,000 hits/day. The "call only when needed"
   constraint is unchanged; only the margin is wider than assumed.

---

## 11. What follows

- **P2** — bitemporal observation store: write and read paths against §4.8,
  raw fetch with all four adjustment flags false, point-in-time queries.
- **P3** — corporate-action ingestion from the P0 §5 bulk fields, versioned so
  that amendments and cancellations are new rows keyed on `bbg_action_id`.
- **P4** — the adjustment engine over `EQY_DVD_ADJUST_FACT`, with retroactive
  recalculation of derived layers.
- **P5** — funds: share classes, conversions, mergers, holdings transformation.

Each gets its own design, spec and plan.
