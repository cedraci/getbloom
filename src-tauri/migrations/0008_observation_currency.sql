-- P7/P2: currency becomes a dimension of the observation itself, not just a
-- resolution-time attribute. Stored VERBATIM from Bloomberg's CRNCY -- an
-- LSE line quoted in pence carries 'GBp', because raw storage records what
-- the number IS, never converts it. NULL for text values and for rows whose
-- instrument has no known currency.
ALTER TABLE observation ADD COLUMN currency TEXT;

-- Backfill existing numeric rows from the currently-believed currency
-- attribute valid at each row's own date.
UPDATE observation o SET currency = a.value
  FROM instrument_attr a
 WHERE a.instrument_id = o.instrument_id
   AND a.attr = 'currency'
   AND a.system_to = 'infinity'
   AND a.valid_from <= o.obs_date AND a.valid_to > o.obs_date
   AND o.value_num IS NOT NULL;

-- From here on, currency is as immutable as the value it prices: a
-- redenomination closes the row and inserts a new one (ingest raises
-- currency_changed when it does).
CREATE OR REPLACE FUNCTION observation_append_only() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value_num IS DISTINCT FROM OLD.value_num
     OR NEW.value_text IS DISTINCT FROM OLD.value_text
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.field_id <> OLD.field_id
     OR NEW.obs_date <> OLD.obs_date
     OR NEW.obs_time IS DISTINCT FROM OLD.obs_time
     OR NEW.granularity <> OLD.granularity
     OR NEW.layer <> OLD.layer
     OR NEW.basis_id IS DISTINCT FROM OLD.basis_id
     OR NEW.run_id <> OLD.run_id
     OR NEW.currency IS DISTINCT FROM OLD.currency THEN
    RAISE EXCEPTION
      'observations are append-only; close system_to and insert a corrected row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;
