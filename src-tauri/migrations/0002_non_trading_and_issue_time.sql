-- Evidence-based non-trading days. No external holiday calendar exists in
-- this system (design decision, 2026-08-13 spec §5.2); instead, a day is
-- recorded here when Bloomberg itself answered "no trading session" --
-- either a dated no_data on a single-day run, or a silent omission inside a
-- multi-day ACTIVE_DAYS_ONLY range that returned neighbours. detect_gaps
-- treats these dates as covered, which is what stops a holiday from being a
-- permanent, un-backfillable phantom gap.
CREATE TABLE non_trading_day (
  instrument_id BIGINT NOT NULL REFERENCES instrument(instrument_id),
  obs_date      DATE NOT NULL,
  source        TEXT NOT NULL DEFAULT 'no_data'
                CHECK (source IN ('no_data','range_inference')),
  recorded_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (instrument_id, obs_date)
);

-- When was this issue raised? Needed by the auto-re-resolve cooldown (an
-- instrument is probed at most once per cooldown window), and honest for the
-- UI regardless.
ALTER TABLE ingest_issue
  ADD COLUMN created_at TIMESTAMPTZ NOT NULL DEFAULT now();
