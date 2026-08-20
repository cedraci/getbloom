-- P3: corporate actions, snapshot-diffed per refresh (design:
-- docs/superpowers/specs/2026-08-20-p3-corporate-actions-design.md).
-- Bloomberg reports the FULL history on every call; a refresh diffs it
-- against the current rows. `payload` is the verbatim Bloomberg row and the
-- authority; the typed columns are best-effort extractions for P4 and the
-- UI (DVD_HIST_ALL_WITH_AMT_STATUS column names have no P0 capture, so the
-- extraction may be partial until the first live run pins them).
CREATE TABLE corp_action (
  id            BIGSERIAL PRIMARY KEY,
  instrument_id BIGINT NOT NULL REFERENCES instrument(instrument_id),
  source_field  TEXT NOT NULL CHECK (source_field IN
                  ('EQY_DVD_ADJUST_FACT','DVD_HIST_ALL_WITH_AMT_STATUS')),
  natural_key   TEXT NOT NULL,
  event_date    DATE,               -- adjustment date / ex-date
  amount        DOUBLE PRECISION,   -- factor / dividend amount per share
  operator      SMALLINT,           -- 1=div 2=mult 3=add; OPPOSITE for volume (P0 10.1)
  flag          SMALLINT,           -- 1=prices only, 3=prices and volumes
  dvd_type      TEXT,
  frequency     TEXT,
  declared_date DATE,
  record_date   DATE,
  pay_date      DATE,
  amount_status TEXT,               -- estimated vs confirmed (the _WITH_AMT_STATUS point)
  payload       JSONB NOT NULL,
  system_from   TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to     TIMESTAMPTZ NOT NULL DEFAULT 'infinity'
);

CREATE UNIQUE INDEX corp_action_current
  ON corp_action (instrument_id, source_field, natural_key)
  WHERE system_to = 'infinity';
CREATE INDEX corp_action_by_instrument ON corp_action (instrument_id, event_date);

CREATE FUNCTION corp_action_append_only() RETURNS trigger AS $fn$
BEGIN
  IF NEW.instrument_id <> OLD.instrument_id
     OR NEW.source_field <> OLD.source_field
     OR NEW.natural_key <> OLD.natural_key
     OR NEW.payload IS DISTINCT FROM OLD.payload
     OR NEW.event_date IS DISTINCT FROM OLD.event_date
     OR NEW.amount IS DISTINCT FROM OLD.amount THEN
    RAISE EXCEPTION
      'corp_action rows are append-only; close system_to and insert';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER corp_action_append_only BEFORE UPDATE ON corp_action
  FOR EACH ROW EXECUTE FUNCTION corp_action_append_only();
