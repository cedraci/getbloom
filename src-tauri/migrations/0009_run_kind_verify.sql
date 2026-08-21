-- P8: the weekly verification pass gets its own run kind. 'backfill' in the
-- run history reads as a manual catch-up; a scheduled verify is neither
-- manual nor a catch-up, and a person scanning the history should see it
-- for what it is.
ALTER TABLE run DROP CONSTRAINT run_kind_check;
ALTER TABLE run ADD CONSTRAINT run_kind_check
  CHECK (kind IN ('eod','backfill','verify'));

-- Retag history: a scheduled backfill could only ever have been the weekly
-- verify -- manual backfills carry trigger_kind 'manual'.
UPDATE run SET kind = 'verify'
 WHERE kind = 'backfill' AND trigger_kind = 'scheduled';
