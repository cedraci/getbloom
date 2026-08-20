-- P6 (design: docs/superpowers/specs/2026-08-20-p6-merger-lifecycle-design.md):
-- merger terms asserted by Bloomberg at the Action-ID level.
--
-- exchange_ratio is Bloomberg's r from CA_MA_STOCK_TERMS ("1.7234 Aqr
-- sh./Tgt sh."): ACQUIRER shares per TARGET share. Stitching converts a
-- predecessor price into successor units by DIVIDING by r -- pinned live on
-- XLNX/AMD (194.92 / 1.7234 = 113.1 vs AMD's 114.27 first close).
--
-- terms is the verbatim CA_MA payload, the same authority-vs-extraction
-- split as corp_action.payload: the typed column is a best-effort reading,
-- the JSONB is what Bloomberg actually said.
ALTER TABLE instrument_link
  ADD COLUMN exchange_ratio DOUBLE PRECISION CHECK (exchange_ratio > 0),
  ADD COLUMN terms JSONB;
