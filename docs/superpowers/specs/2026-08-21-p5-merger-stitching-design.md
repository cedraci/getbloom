# P5 — Fund-merger stitching (design)

**Date:** 2026-08-21
**Status:** APPROVED (user, 2026-08-21: "go for P5")
**Branch:** `bloomberg-security-master`
**Depends on:** P4 `2026-08-21-p4-adjustment-engine-design.md` (per-segment
adjustment), `instrument_link` (0001_init.sql: proposal/confirm flow, P0 §7.2
"no Bloomberg field returns a successor").

## 1. Objective

The last founding requirement: **fund merges must not break timeseries.**
When fund A is absorbed into fund B, a confirmed `instrument_link`
(A → B) exists — but nothing follows it. P5 makes the surviving
instrument's series extend **backward through confirmed links**, derived on
read like P4: query B, get B's history, spliced with A's history before the
effective date, in B-share units. Nothing stored, ever.

## 2. The splice

At a junction with effective date `D`:

- the successor's segment covers `obs_date >= D`; the predecessor's covers
  `obs_date < D`;
- predecessor values are scaled by a **splice ratio** so the series is
  continuous in successor units:
  `ratio = first successor value at/after D ÷ last predecessor value before D`
  (both taken AFTER the segment's own P4 adjustment in the requested mode,
  so the ratio and the values agree);
- `rename` and `share_class_change` links are the same fund under a new
  name: **ratio = 1**, no junction noise;
- `merger` and `conversion` links use the derived ratio — the exchange
  ratio is not a Bloomberg field, but price continuity at the junction IS
  the evidence the database holds. The ratio used is always reported;
- `spinoff` links are **never stitched**: a child's history is not the
  parent's.

Volume-kind fields (mnemonic contains VOLUME) are not comparable across a
merger; their segments concatenate **unscaled** and the segment report says
so.

## 3. The chain walk

Starting from the requested instrument, follow confirmed links backward
(`successor_id = current`), excluding spinoffs:

- at each step take the link with the **latest effective_date** strictly
  before the current segment's start (the junction dates must descend —
  a link "inside" an already-covered range is a data error, reported);
- if two confirmed links tie on that effective_date, **stop** there and
  report the ambiguity — silently picking one predecessor would fabricate
  history;
- a cycle guard (seen-set) stops loops; depth is bounded to 10 links;
- a junction whose ratio cannot be derived (either side has no observation
  near `D`) stops the walk there, with the reason in the segment report.

Every stitched result carries a **segment list**: instrument, label, date
range, link type, ratio applied (or why the walk stopped). A spliced series
that cannot say where its numbers came from is not an accurate timeseries.

## 4. Shape

- `src-tauri/src/stitch.rs`: pure chain-planning over link rows
  (unit-testable) + `stitched_series(pool, instrument_id, field_id, mode,
  limit)` composing P4's `adjusted_series` per segment.
- Command `list_stitched` + `export_stitched_csv`
  (`obs_date,value,source`).
- Data tab: an **"Extend through confirmed mergers"** checkbox (visible
  when the instrument has confirmed predecessor links); the table gains a
  Source column on foreign rows; a thin segment report line. CSV export
  switches to the stitched exporter.

## 5. Non-goals

- No automatic link confirmation — the human review gate stands (P0 §7.2).
- No manual exchange-ratio entry (the derived ratio is reported; an
  override column can be added later if a real case demands it).
- No CA_MA_* merger-evidence fetching — link proposals keep coming from
  identifier history and the user.
- No forward stitching (querying the dead fund does not continue into the
  survivor: the surviving instrument is the timeseries).
