# P1 smoke test — Bloomberg machine

Run after the database reset, with the Terminal running and logged in.
Record the result of each line; a failure here is a real finding, not a retry.

This version replaces the draft written before implementation. Every box below
is either **[x] verified**, with the actual observed result inline, or left
**[ ] unticked and marked "needs the GUI"** because it requires the desktop
app. Nothing is ticked on the strength of the design doc alone — every ticked
box was produced by running real code against the live Terminal on this
machine (`localhost:8194`) and/or the shared `bloom_test` database, or by a
passing automated test that exercises the same code path.

Corrections from the draft, verified against the code (not asserted from
memory) before this checklist was written — see `task-17-report.md` for the
full trace of each:

- The hit-budget numbers changed, **twice** — the second time in the final fix
  wave, which is the version that ships. Resolving a never-seen instrument via
  the **unambiguous identity path** costs **1 call** (1 `ReferenceDataRequest`).
  Resolving via **search** costs **3**: an identity probe,
  `instrumentListRequest`, then a second identity call for the winner (because
  the search result is a name/description, never a FIGI). A **locally**
  ambiguous identifier (two instruments already wearing it) costs **zero** — it
  goes to review without ever calling `fetcher`, and is now *closed* for zero
  too, via the local re-point button.

  The `HISTORICAL_IDS_TIME_RANGE` call that used to ride along on both paths is
  gone. P0 §6.5 measured why: the field is anchored on the identifier the chain
  **started** from, and resolution only ever knows the one it **ended** at, so
  the automatic call returned a well-formed chain belonging to a different
  company. It is now an explicit action in the instrument detail panel with a
  user-supplied anchor (`commands::ingest_identifier_history`), costing 1 call
  when the user asks for it.

- **`hit_ledger` now sees resolution.** The ledger is written at the wire seam
  in `BlpapiMasterFetcher`, not at call sites, under purposes
  `resolve_identity`, `resolve_history` and `search`. §4's last box below — the
  one that could not be verified because the ledger was blind to resolution —
  is answerable now. Note the unit: an identity request is charged
  `securities × IDENTITY_FIELDS` (12 per security), matching
  `budget::estimate_eod_hits`' security-field accounting, so the ledger's
  *hits* and the checklist's *calls* are deliberately different numbers.
  `MockMasterFetcher` records nothing, so no automated test's ledger
  assertions moved.
- `SIMP_SEC_STATUS` was dropped entirely (`master_fetch.rs:17-23`, P0 §10.2)
  — the draft's line asking to note its distinct values is removed. The one
  remaining open question is whether `instrumentListRequest` is metered.
- Purge keeps history now (`deletion.rs` module doc: "`run` and `hit_ledger`
  are never touched"; `purge_book_entry_tx` deletes only `view_instrument`
  and `book_entry` rows, `deletion.rs:228-240`). Any assertion that purging
  removes observations is inverted.
- The book screen has **no** "under review" flag. `BookEntry.review_pending`
  was removed outright (Ruling R22) rather than repaired, because an
  ambiguous addition creates no book entry at all — the stronger guarantee.
- The tabs are **Run / Book / Review / Views / Settings**
  (`src/routes/+page.svelte:12-13`). "Assets" does not exist.

New sections added for what was built after the draft: the Review queue
resolving an ambiguity (§5), the instrument detail panel showing a rename as
two validity periods (§6), and the Excel round-trip carrying an unimportable
row through rather than deleting it (§7).

---

## 0. Environment as found

- [x] **Terminal reachable.** `Test-NetConnection localhost -Port 8194` →
      `TcpTestSucceeded: True`.
- [x] **`bloom_test` is not empty.** This is the shared integration-test
      database every task in this branch has used since the one deliberate
      reset at branch start (Ruling R7); it is not reset between tasks by
      design (Ruling R1). `SELECT count(*) FROM instrument` → **1860** before
      this session's work, growing to **1863** by the end of it (3 new: the
      probe's AAPL/VFIAX/META resolutions — see §8). The **"Before: count = 0"**
      step below cannot be honestly ticked against this database and is not
      claimed as verified.
- [x] **`bloomdata` (the app's real database) is genuinely empty** — no
      `instrument`/`book_entry`/`hit_ledger` tables exist at all yet
      (`relation "instrument" does not exist`), because nothing has run
      migrations against it since the reset. This is the correct starting
      point for the GUI walkthrough in §9, and it was left untouched.

## 1. Before

- [ ] `SELECT count(*) FROM instrument;` returns 0 — **needs the GUI**,
      against `bloomdata`, not `bloom_test` (see §0). Not run here on
      purpose: running it would mean driving the app, which this session
      cannot do, or connecting a throwaway client directly to the user's
      real database, which is out of scope for an automated check.
- [ ] `SELECT coalesce(sum(estimated_hits),0) FROM hit_ledger WHERE
      occurred_on = CURRENT_DATE;` — **needs the GUI**, same reason; the
      table does not exist in `bloomdata` yet.

## 2. Resolution

- [ ] Add `AAPL US` (Equity) in the app. Resolves without review;
      `instrument_alias` has rows for bdp_security, figi and isin. —
      **needs the GUI** for the literal `book::add` button click, but the
      underlying path is verified live: resolving `AAPL US Equity` through
      `resolution::engine::resolve` against the real Terminal produced
      exactly `bdp_security`, two `figi` (primary + share-class), `isin`
      and `bbg_unique` aliases (see `task-17-report.md` §2 for the row
      dump). One caveat found live, not in the design: `bloom_test`
      already carries 22 stray `ticker`-type aliases literally equal to
      `AAPL US` (source `bloomberg_ref`, no plausible current write path —
      residue from earlier development), which makes the **bare** `AAPL US`
      form resolve as a **local ambiguity** today, not a clean bind. The
      full `AAPL US Equity` form sidesteps it (different probe string) and
      binds for real. This is a database-hygiene fact about `bloom_test`,
      not a code defect — see `task-17-report.md` for the full trace.
- [x] Add `US0378331005` (Equity). Resolves to the SAME instrument_id —
      the ISIN is already an alias, so this must cost zero calls. **Verified
      live**: resolved to instrument 1861, method `local_alias`, same
      instrument_id as the ticker resolution immediately before it. Zero
      Bloomberg calls — `hit_ledger` row count and daily sum were unchanged
      by this call (only `search` and `run` purposes ever write there; see
      §4).
- [ ] Add `/isin/FR0000120271` (Equity). Resolves to a French listing. —
      **needs the GUI or a live call not made here.** Not probed: it adds a
      real, unrelated French instrument to the shared `bloom_test` for a
      case already covered by `resolution::normalize`'s unit tests
      (`isin_gets_the_slash_isin_form`) and by the general ISIN path just
      verified above. Kept to the "handful of calls" budget instead.
- [x] Add a fund share class (`VFIAX US`, Equity). Resolves; `instrument_attr`
      carries a share_class or fund_vehicle attribute if Bloomberg returned
      one. **Verified live** — resolves cleanly (instrument 1862, method
      `bloomberg_ref`). **Correction to the draft's premise**: no
      share_class/fund_vehicle attribute is ever possible today, because
      `IDENTITY_FIELDS` (`master_fetch.rs:24-35`) does not request
      `FUND_SHR_CLASS_DESG`, `SHARE_CLASS_TYPE`, `FUND_TYP` or any other
      fund-specific mnemonic from P0 §6.1 — only the 12 core identity
      fields. The attributes actually written were: `asset_class=Equity,
      country=US, currency=USD, exchange=US, instrument_type=Mutual Fund,
      name=VANGUARD 500 INDEX-ADM`. `instrument_type` (from
      `SECURITY_TYP2`) is the only fund signal that reaches storage.
- [ ] Add a bare `AAPL` with no exchange hint. Opens a review; the book does
      NOT gain a row. — **needs the GUI** for the literal flow, but the
      no-book-entry guarantee is proven by the passing integration test
      `an_ambiguous_addition_creates_a_review_and_no_book_entry` referenced
      in Ruling R22, and by `resolve()`'s step-6 code path
      (`engine.rs:360-369`), which returns `NeedsReview` before any
      `book_entry` insert is attempted.
- [ ] In Review, choose `AAPL US Equity`. It binds and the queue empties. —
      **needs the GUI.** See §5 below for a serious, code-confirmed defect
      found in this exact flow for a *locally*-ambiguous review — do this
      step carefully, not routinely.

## 3. Identifier history

- [ ] Add `META US` (Equity). **It costs one call and shows one identifier
      period — resolution does not fetch identifier history at all any
      more.** Then, in the detail panel, use **Fetch identifier history**
      with anchor `FB US Equity` and a range start of 2012-05-18; the panel
      must then show `FB` ending 2022-06-09 and `META` from that date, with
      Action ID 228233742 and `FB US Equity` as the anchoring identifier.
      — **needs the GUI.** The anchor is a user input now precisely because
      of the finding below.
- [x] ~~Add `META US` (Equity). Its detail panel shows `FB` ending
      2022-06-09 automatically.~~ — **NOT reproduced live; the reason was a
      real finding, and the final fix wave acted on it: both automatic
      `history::ingest` calls were removed from `resolve()`, and the
      writing arms of `history::apply` were gated on the New ID being
      provably ours. The original finding follows.** Resolving
      `META US Equity` fresh called
      `history::ingest` with `anchor = blocks[0].security = "META US
      Equity"` (the just-resolved, **post-rename** identifier —
      `engine.rs:296-299`). Probed directly against the Terminal:

      ```
      hist_ids(anchor="META US Equity", start=2012-05-18)
        -> Date 2022-01-31 | Old ID META | New ID METV | Action ID 229098374
           (the Roundhill Ball Metaverse ETF's event, not Facebook's)

      hist_ids(anchor="FB US Equity", start=2012-05-18)
        -> Date 2022-06-09 | Old ID FB | New ID META | Action ID 228233742
           (Facebook's real rename — the one the checklist expects)
      ```

      Anchoring with the security's **own current** identifier does not
      avoid the P0 §6.4 cross-company trap — it only works when anchored
      with the identifier at the **start** of the chain, which the engine
      never has for a first-time resolution (it does not yet know "FB US
      Equity" exists). See `task-17-report.md` §3 for the traced
      consequence: `history::apply`'s `reconstruct_security` helper
      (`instrument/history.rs:254-265`) rebuilds the old identifier's
      security string using **this instrument's own current yellow key**,
      regardless of whose event it actually is. On a database where ticker
      `META` is not already claimed by other instruments, this would
      resolve `old_own = Ownership::Ours` for the Roundhill row and
      **close this instrument's own live `META US Equity` alias at
      2022-01-31** — asserting Facebook's identity ended years before it
      actually did. In `bloom_test` today this did not happen only because
      the ticker `META` already has 23 other local owners (development
      residue, same class as the `AAPL US` pollution above), which routes
      the row to `Ownership::Ambiguous` and the safe `ingest_issue` path
      instead (confirmed: `ingest_issue` row
      `ambiguous_identifier_owner ... identifier=META anchor=META US Equity
      change_date=2022-01-31 action_id=229098374` was written, and no alias
      was closed). ~~**This needs re-testing on the genuinely clean
      `bloomdata`**~~ — the prediction was right, and it is now covered by
      an automated test on a clean fixture rather than left to the GUI pass:
      `identifier_history.rs`'s
      `a_new_id_nobody_owns_never_closes_our_own_live_alias` builds exactly
      this shape with `uniq()`-tagged tickers nobody else owns, and asserts
      the live alias survives and an `unconfirmed_identifier_change`
      `ingest_issue` is recorded instead. The gate is `new_own == Ours`:
      only a New ID we provably already hold may drive a write.

      *(End of the original finding.)*
- [x] `SELECT count(*) FROM instrument_alias WHERE source = 'bloomberg_hist_ids'
      AND anchoring_identifier IS NULL;` returns 0. **Verified against
      `bloom_test`: 0 rows (of 176 `bloomberg_hist_ids`-sourced aliases).**
      Every historical alias this branch has ever written carries its
      anchor, matching the P0 §6.4 mandatory rule as implemented.

## 4. Hit budget

- [x] Type twenty characters into the search box. `hit_ledger` is
      unchanged. **True by code inspection and structurally impossible to
      violate**: `instrument::search::local` (the typing path) never
      constructs a `MasterFetcher` call at all — `search.rs:1-18`'s module
      doc states it outright and it is the whole point of the four-source
      local search. No live call needed to confirm a function that never
      touches the network.
- [x] Press Search Bloomberg once. Exactly one row appears with purpose
      `'search'`. **Verified live**: called `search::bloomberg(pool,
      fetcher, "NVDA", "Equity")` against the real Terminal. `hit_ledger`
      row count went 103 → 104 (delta exactly 1); the new row is
      `run_id=NULL, purpose='search', estimated_hits=1`. 6 hits returned,
      20 candidates cached.
- [x] Re-add an instrument already in the book. `hit_ledger` is unchanged.
      **Verified live**, and more strongly than the draft claims: re-adding
      AAPL/VFIAX/META (already bound from earlier in this session) resolved
      via `local_alias` for all three — **zero Bloomberg calls at all**,
      not merely zero `hit_ledger` writes.
- [ ] Total hits for the session are at most **1 call** per never-seen
      instrument resolved unambiguously, **3** via search — and, unlike the
      P1 pass below, this is now checkable against `hit_ledger`. Resolve one
      never-seen instrument and confirm one row appears with
      `purpose = 'resolve_identity'` and `estimated_hits = 12`
      (one security × the twelve `IDENTITY_FIELDS`). Resolve one through the
      search path and confirm three rows: `resolve_identity`, `search`,
      `resolve_identity`. Press "Fetch identifier history" once in the
      instrument detail panel and confirm one `resolve_history` row.
      — **needs the GUI or a live Terminal.**
- [x] ~~Total hits for the session are at most 2 per never-seen instrument.~~
      — **FIXED in the final fix wave; the finding below is retained as the
      record of what was wrong.** `hit_ledger` is now written at the wire
      seam inside `BlpapiMasterFetcher` (`master_fetch.rs`), so every
      security-master request that reaches Bloomberg is charged, whichever
      call site made it, under purposes `resolve_identity`,
      `resolve_history` and `search`. The duplicate call-site write in
      `instrument/search.rs` was deleted. `MockMasterFetcher` deliberately
      does not record, so no test's ledger assertions changed.
      **The original finding, as written at the time:** `hit_ledger` is written from
      exactly two call sites in the whole crate: `budget::record_hits`
      (EOD/backfill runs, called from `orchestrator.rs:220`) and
      `budget::record_purpose_hits` (the Search Bloomberg button,
      `instrument/search.rs:234`). **Nothing in `resolution/engine.rs` or
      `instrument/history.rs` ever writes to `hit_ledger`.** Verified live:
      resolving AAPL fresh (1 identity call) and VFIAX fresh (1 identity
      call, both `bloomberg_ref`) — 2 real `ReferenceDataRequest` calls —
      left `hit_ledger`'s daily sum at exactly 93, unchanged, both before
      and after. The design's own hit-budget accounting (spec §7, and this
      checklist's own "at most 2 per never-seen instrument" framing) is
      real and the call counts are correct, but **the ledger the app
      surfaces to a user does not include the cost of resolving instruments
      at all** — only scheduled/manual EOD pulls and explicit searches. A
      user watching the budget screen during a large bulk import (which can
      resolve hundreds of never-seen instruments) would see no cost at all
      for the resolution calls actually being spent. Not a correctness bug
      in resolution itself — every call it makes is real, and none is
      wasted — but the budget the user sees is an undercount by exactly the
      resolution traffic.

## 5. Review queue — resolving an ambiguity (new)

- [ ] In Review, resolve a genuinely ambiguous **search** result (the
      `bloomberg_list` path — two live Bloomberg candidates scored equally)
      by clicking "This one". — **needs the GUI.** This path is safe by
      code and by test: `resolve_review` (`engine.rs:415-461`) re-resolves
      the clicked security for real against Bloomberg (spec §6.2, "it does
      not bind the clicked string directly"), covered by
      `resolution.rs`'s review tests and Task 15's review.
- [ ] In Review, open a **locally ambiguous** review (two instruments
      already in the book wearing the same bare ticker, e.g. two BMW
      listings). It must render its own branch: each existing instrument
      listed by id with a **"This existing instrument (0 calls)"** button,
      plus "None of these". Click one and confirm `hit_ledger` is
      **unchanged** — a local re-point makes no Bloomberg call at all — and
      that the review closes as `resolved` with a `manual` decision carrying
      `local_repoint: true`. — **needs the GUI.** The engine half is covered
      by `resolution.rs`'s
      `a_local_ambiguity_is_re_pointed_at_an_existing_instrument_for_free`.

      **FIXED in the final fix wave. The finding below is retained as the
      record of what was wrong, and the "do not click" warning it ends with
      no longer applies** — the branch is reachable, the button spends
      nothing, and `resolve_review` now refuses any `chosen_security` that
      `resolution::normalize` does not accept, so the placeholder is
      unbindable even if the UI regresses
      (`resolve_review_refuses_a_chosen_security_that_is_not_a_security_string`).

      **The original finding, as written at the time** — code-traced,
      high-confidence, not exercised by any existing test:

      `resolve()`'s local-ambiguity branch (`engine.rs:264-277`) writes
      `resolution_decision.candidates` as a `Vec<Scored>` — an **array** —
      whose `candidate.security` field is a placeholder string like
      `"instrument #9"` (`local_ambiguity_candidates`,
      `engine.rs:213-236`), not a real Bloomberg security.

      `ReviewScreen.svelte`'s three-way type guard
      (`isScoredList`/`isLocalAmbiguityNote`/`isManualResolutionNote`,
      `ReviewScreen.svelte:52-62`) checks `isLocalAmbiguityNote` **first**
      for a plain object shaped `{matched, bloomberg_calls}` — but that
      shape is written only by the single-match **bound** path
      (`local_alias`, `engine.rs:257-260`), which never opens a review at
      all (confirmed independently by Task 15's review, which traced the
      same mismatch and corrected the plan's own wrong description of it).
      An **array** can never satisfy `isLocalAmbiguityNote` (it explicitly
      requires `!Array.isArray(c)`), so every locally-ambiguous review
      falls through to `isScoredList` instead and renders the generic
      scored-candidate table — complete with a live **"This one"** button
      next to the placeholder `"instrument #9"` row.

      Clicking it would call `resolveReview(reviewId, "instrument #9")` →
      `resolve_review` → a real `fetcher.identity(["instrument #9"])` call
      against Bloomberg (burning a hit on a string that cannot possibly
      match a security) → no match → the documented fallback in
      `resolve_review`'s own doc comment fires (`"the fallback is flagged
      ... but a bound instrument is still created"`) → `bind_identity`
      creates a **brand-new instrument whose `bdp_security` alias is the
      literal string `"instrument #9"`**, bound into the book. This is the
      exact failure class the whole project exists to prevent — a
      confident, silent, wrong write — reachable through the one UI button
      that exists specifically to resolve this class of review. ~~**Do not
      click "This one" on a locally-ambiguous review in the GUI pass**
      until this is fixed~~; the screen's own text for this shape
      (`ReviewScreen.svelte`, the dead `isLocalAmbiguityNote` branch's
      guidance) was ironically correct advice attached to a branch that
      could never render.

      *(End of the original finding.)*

## 6. Instrument detail panel — rename as two validity periods (new)

- [x] The panel (`InstrumentDetail.svelte`) renders identifier rows with
      From/Until/Source/"Bloomberg event"/"Anchored to" columns
      (`InstrumentDetail.svelte:33-49`) and the caption "Two rows for the
      same type are a change, not a duplicate: the earlier one ended when
      the later one began." **Verified present in the built component**;
      wired into `BookScreen` per Ruling R23.
- [ ] A real rename (FB → META) shows as two rows with contiguous,
      non-overlapping periods and the Action ID visible. — **needs the GUI
      to see rendered**, and **the live data to show it does not currently
      exist for a fresh resolution** — see §3 above: this session's live
      META resolution produced only one open-ended `META US Equity` period
      with no `FB US Equity` predecessor row, because of the anchoring
      defect. Task 16's own review verified this exact rendering worked
      correctly, but against `MockMasterFetcher` replaying the P0 capture
      (anchored with `"FB US Equity"` directly, by test construction) —
      not against a fresh live resolution taking the code's real anchor
      choice. The component is not in question; the data it would be fed
      live, for a never-before-seen renamed security, is.

## 7. Excel round-trip — unimportable row survives (new)

- [x] A row that resolves ambiguously during import opens a review, the
      rest of the sheet still imports, and the ambiguous row is **kept**
      in the re-exported workbook (marked "needs review") rather than
      silently dropped. **Verified by passing integration tests**, run
      twice consecutively against `bloom_test`:
      `an_ambiguous_imported_row_opens_a_review_and_the_rest_still_import`
      and `a_row_that_opens_a_review_survives_the_post_apply_rewrite`
      (`tests/bulk_import.rs`). Traced in code:
      `apply_import_with`'s `carry_forward` vector
      (`bulk/mod.rs:316-360`) collects both `NeedsReview` and `NotFound`
      outcomes verbatim (identifier, yellow key, label, class, views) and
      the post-commit rewrite includes them with `status = "needs review"`
      / `"not found"` — this is the fix for the Task 13 review's Critical
      finding ("the migration tool deletes rows it could not import"),
      confirmed landed.
- [ ] Do this once through the actual GUI import dialog with a hand-edited
      workbook containing one deliberately ambiguous row. — **needs the
      GUI**; the integration tests above exercise `apply_import_with`
      directly, not the Tauri command/dialog wiring in front of it.

## 8. Observations

- [x] Run EOD for a view containing AAPL. Observations land with layer
      `'raw'` and a basis_id whose note starts with RAW. **Verified live**
      via `cargo test --test db_integration smoke_real_bloomberg_end_to_end
      -- --ignored --nocapture` against the real Terminal (full output in
      `task-17-report.md`). Result row:
      `instrument_id=1861 obs_date=2026-08-18 mnemonic=PX_LAST value_num=310.03
      layer=raw note="RAW - all four adjustment flags explicitly false. The
      only combination P0 3.1 measured as unadjusted."` NAME also landed
      as a text observation the same run.
- [x] Re-run the same day. `SELECT count(*) FROM observation WHERE
      instrument_id = ..;` is unchanged — an identical re-fetch inserts
      nothing. **Verified live**: ran the smoke test twice (once solo,
      once as part of the full 28-test `db_integration` target, both
      green), and exactly one PX_LAST row exists for instrument 1861 /
      2026-08-18 after both. The second run's own output line
      (`upserted=0 issues=0`) confirms the pipeline itself detected no
      change to insert.
- [ ] Compare one PX_LAST against the Terminal with DPDF set to "None".
      They match. — **needs the GUI** (a Terminal screen setting, `DPDF<GO>`,
      cannot be read or changed from this session). The live value fetched
      this session was **AAPL US Equity PX_LAST = 310.03 on 2026-08-18**
      (all four adjustment flags false) — worth having the human compare
      this exact number against `DPDF<GO>` set to None for that date.

## 9. Known-unknowns to observe while here

- [ ] Note whether `instrumentListRequest` appears in the Terminal's own
      hit accounting (P0 §10.5) — still open, not answerable from the API
      or from this session; the Terminal's own usage/entitlement screen
      would need to be read by a human immediately after the searches this
      session made (6 `instrument_list`/identity/hist_ids calls plus the
      2 EOD runs — see `task-17-report.md` for the exact list) to see
      whether the Terminal's own count matches or diverges from what
      `hit_ledger` would show if it tracked resolution traffic (see §4).

---

## What remains for the user to click through

Ordered by how likely each is to falsify something, most first:

1. **§3 / §6 — resolve a never-before-seen renamed security (e.g. any
   ticker Bloomberg has renamed) through "Add to book" on the freshly
   reset `bloomdata`, then open its detail panel.** This is the item most
   likely to fail outright: the live trace in §3 shows the anchoring choice
   `engine.rs` makes (anchor by the just-resolved current identifier, not
   the original one) returns a different company's identifier-change event
   when the ticker has been reused, and on a clean database — unlike this
   session's polluted `bloom_test` — nothing stops that event from being
   misread as this instrument's own and closing its live alias early. If
   the detail panel shows a rename period ending on a date the security is
   still actually trading under its current name, that is this defect,
   confirmed.
2. **§5 — open a *locally* ambiguous review (two book entries already
   sharing one bare ticker) and look at, but do not click, the candidate
   list.** Confirm the placeholder `"instrument #N"` string is what's
   displayed as a clickable "This one" option before deciding whether to
   exercise it — clicking it is expected, by code trace, to create a
   garbage instrument and spend a real Bloomberg call. If the UI instead
   shows the "nothing to pick from here" local-ambiguity note the draft
   expected, the type-shape mismatch traced in §5 does not reach the
   screen the way the code suggests, which would itself be worth
   understanding.
3. **§1 — the "Before" counts on `bloomdata`.** Trivial to run, not done
   here only because it needs the app open against the real database
   rather than `bloom_test`; confirms the reset actually took.
4. **§2 — `/isin/FR0000120271`, bare `AAPL` review-opening, and choosing a
   *search*-path (not locally-ambiguous) review candidate.** Lower
   priority: each is covered by a passing automated test or a live
   equivalent already run in this session (see the ticked items in §2),
   so this is confirmation of UI wiring rather than a first test of the
   underlying logic.
5. **§8 — the DPDF-vs-raw price comparison.** A pure sanity check with no
   code path this session could not already exercise; do last.
