-- P7: the quality gate and the weekly verification re-fetch.
--
-- 'quality' is a third severity, distinct on purpose: 'warn' means something
-- did not arrive, 'error' means something failed, 'quality' means a value
-- ARRIVED cleanly and still looks wrong (non-positive price, outlier jump,
-- frozen series, unexplained silence). A reader triaging a run needs the
-- distinction: 'quality' rows point at data to distrust, not plumbing to fix.
ALTER TABLE ingest_issue DROP CONSTRAINT ingest_issue_severity_check;
ALTER TABLE ingest_issue ADD CONSTRAINT ingest_issue_severity_check
  CHECK (severity IN ('warn','error','quality'));

-- Per-field quality thresholds. Data-driven like the rest of field_def
-- (adding a check stays an UPDATE, never a code change), and opt-in per
-- field because applicability depends on what the field IS: a price is
-- never negative, a yield or a spread legitimately is; an FX fix moving 30%
-- is a broken tape, an equity moving 30% is a Tuesday in small caps.
--   qc_nonpositive: flag value_num <= 0.
--   qc_outlier_pct: flag |day-over-day move| above this percentage.
--   qc_stale_days:  flag a value repeated this many consecutive observations.
ALTER TABLE field_def ADD COLUMN qc_nonpositive BOOLEAN NOT NULL DEFAULT FALSE;
ALTER TABLE field_def ADD COLUMN qc_outlier_pct DOUBLE PRECISION
  CONSTRAINT field_def_qc_outlier_positive
  CHECK (qc_outlier_pct IS NULL OR qc_outlier_pct > 0);
ALTER TABLE field_def ADD COLUMN qc_stale_days INTEGER
  CONSTRAINT field_def_qc_stale_min
  CHECK (qc_stale_days IS NULL OR qc_stale_days >= 2);

-- Weekly verification re-fetch: on this ISO weekday (1=Mon..7=Sun, NULL=off)
-- the scheduled run covers the trailing 5 weekdays instead of one, so an
-- upstream restatement is actually re-read -- ingest supersedes the old row
-- and (P7) says so. Defaults ON (Friday): a restatement detector that ships
-- disabled detects nothing.
ALTER TABLE schedule ADD COLUMN verify_dow SMALLINT DEFAULT 5
  CONSTRAINT schedule_verify_dow_range
  CHECK (verify_dow IS NULL OR verify_dow BETWEEN 1 AND 7);
ALTER TABLE schedule ADD COLUMN last_verified_on DATE;
