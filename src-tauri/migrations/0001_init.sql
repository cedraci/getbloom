-- P1 instrument/security master. Greenfield: this replaces migrations 0001-0004,
-- which is why the database must be dropped and recreated before first run.
--
-- Reading order: identity spine, then everything that hangs off it, then the
-- pipeline tables retained from the previous schema.

CREATE EXTENSION IF NOT EXISTS pg_trgm;

-- ---------------------------------------------------------------- identity

-- The spine. Nothing here changes: no ticker, no ISIN, no name, no status.
-- id_bb_global is nullable because a user may create an instrument before
-- Bloomberg has been asked about it; it is write-once once known.
CREATE TABLE instrument (
  instrument_id  BIGSERIAL PRIMARY KEY,
  id_bb_global   TEXT UNIQUE,
  id_bb_unique   TEXT,
  created_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE FUNCTION instrument_write_once() RETURNS trigger AS $fn$
BEGIN
  IF NEW.instrument_id <> OLD.instrument_id THEN
    RAISE EXCEPTION 'instrument_id is immutable (write-once)';
  END IF;
  IF OLD.id_bb_global IS NOT NULL
     AND NEW.id_bb_global IS DISTINCT FROM OLD.id_bb_global THEN
    RAISE EXCEPTION 'id_bb_global is write-once: % cannot become %',
      OLD.id_bb_global, NEW.id_bb_global;
  END IF;
  IF OLD.id_bb_unique IS NOT NULL
     AND NEW.id_bb_unique IS DISTINCT FROM OLD.id_bb_unique THEN
    RAISE EXCEPTION 'id_bb_unique is write-once';
  END IF;
  IF NEW.created_at <> OLD.created_at THEN
    RAISE EXCEPTION 'created_at is immutable (write-once)';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER instrument_write_once BEFORE UPDATE ON instrument
  FOR EACH ROW EXECUTE FUNCTION instrument_write_once();

-- Every resolution, including the ones that never called Bloomberg. Created
-- before instrument_attr because attributes cite the decision that produced them.
CREATE TABLE resolution_decision (
  id                   BIGSERIAL PRIMARY KEY,
  raw_input            TEXT NOT NULL,
  normalized           TEXT NOT NULL,
  hint_exchange        TEXT,
  hint_country         TEXT,
  hint_currency        TEXT,
  hint_asset_class     TEXT,
  method               TEXT NOT NULL CHECK (method IN
                         ('local_alias','bloomberg_ref','bloomberg_list','manual')),
  chosen_instrument_id BIGINT REFERENCES instrument(instrument_id),
  candidates           JSONB NOT NULL,
  bbg_response         JSONB,
  decided_at           TIMESTAMPTZ NOT NULL DEFAULT now(),
  decided_by           TEXT NOT NULL
);

-- Bitemporal attributes. valid_* is when the fact was true in the world;
-- system_* is when we believed it. A correction closes system_to and inserts.
CREATE TABLE instrument_attr (
  id             BIGSERIAL PRIMARY KEY,
  instrument_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  attr           TEXT NOT NULL CHECK (attr IN
                   ('name','exchange','country','currency','asset_class',
                    'instrument_type','issuer','share_class','fund_vehicle','status')),
  value          TEXT NOT NULL,
  valid_from     DATE NOT NULL,
  valid_to       DATE NOT NULL DEFAULT 'infinity',
  system_from    TIMESTAMPTZ NOT NULL DEFAULT now(),
  system_to      TIMESTAMPTZ NOT NULL DEFAULT 'infinity',
  source         TEXT NOT NULL CHECK (source IN ('bloomberg','user','derived')),
  decision_id    BIGINT REFERENCES resolution_decision(id),
  CONSTRAINT instrument_attr_period CHECK (valid_from < valid_to)
);
CREATE UNIQUE INDEX instrument_attr_current
  ON instrument_attr (instrument_id, attr, valid_from)
  WHERE system_to = 'infinity';

-- Every identifier ever worn. A ticker change closes a row and inserts another;
-- no UPDATE ever touches `value`.
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
  source               TEXT NOT NULL CHECK (source IN
                         ('bloomberg_hist_ids','bloomberg_ref','user')),
  bbg_action_id        TEXT,
  anchoring_identifier TEXT,
  CONSTRAINT instrument_alias_period CHECK (valid_from < valid_to)
);

-- P0 6.4: HISTORICAL_IDS_TIME_RANGE asked about META US Equity returns
-- Facebook's rename or the Roundhill ETF's rename depending on whether
-- HISTORICAL_STARTING_IDENTIFIER was supplied. An alias whose anchor is unknown
-- cannot be trusted, so storing one is made impossible.
ALTER TABLE instrument_alias ADD CONSTRAINT alias_anchor_required
  CHECK (source <> 'bloomberg_hist_ids' OR anchoring_identifier IS NOT NULL);

CREATE INDEX instrument_alias_lookup ON instrument_alias (id_type, lower(value));
CREATE INDEX instrument_alias_by_instrument ON instrument_alias (instrument_id);
CREATE UNIQUE INDEX instrument_alias_current
  ON instrument_alias (instrument_id, id_type, value, valid_from)
  WHERE system_to = 'infinity';
CREATE INDEX instrument_alias_trgm
  ON instrument_alias USING gin (value gin_trgm_ops);

CREATE FUNCTION alias_value_immutable() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value <> OLD.value
     OR NEW.id_type <> OLD.id_type
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.valid_from <> OLD.valid_from
     OR NEW.source <> OLD.source THEN
    RAISE EXCEPTION
      'instrument_alias identity columns are immutable; close valid_to/system_to and insert a new row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER instrument_alias_immutable BEFORE UPDATE ON instrument_alias
  FOR EACH ROW EXECUTE FUNCTION alias_value_immutable();

CREATE FUNCTION attr_value_immutable() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value <> OLD.value
     OR NEW.attr <> OLD.attr
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.valid_from <> OLD.valid_from THEN
    RAISE EXCEPTION
      'instrument_attr identity columns are immutable; close system_to and insert a new row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER instrument_attr_immutable BEFORE UPDATE ON instrument_attr
  FOR EACH ROW EXECUTE FUNCTION attr_value_immutable();

-- P0 7.2: no Bloomberg field returns a successor security, so every link is
-- derived. confirmed_by IS NULL means "proposed"; no query may follow it.
CREATE TABLE instrument_link (
  id              BIGSERIAL PRIMARY KEY,
  predecessor_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  successor_id    BIGINT NOT NULL REFERENCES instrument(instrument_id),
  link_type       TEXT NOT NULL CHECK (link_type IN
                    ('rename','merger','conversion','share_class_change','spinoff')),
  effective_date  DATE NOT NULL,
  evidence        JSONB NOT NULL,
  confirmed_by    TEXT,
  confirmed_at    TIMESTAMPTZ,
  CHECK (predecessor_id <> successor_id),
  CHECK ((confirmed_by IS NULL) = (confirmed_at IS NULL))
);
CREATE INDEX instrument_link_pred ON instrument_link (predecessor_id);
CREATE INDEX instrument_link_succ ON instrument_link (successor_id);

CREATE TABLE resolution_review (
  id            BIGSERIAL PRIMARY KEY,
  decision_id   BIGINT NOT NULL REFERENCES resolution_decision(id),
  status        TEXT NOT NULL CHECK (status IN ('pending','resolved','rejected')),
  opened_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  closed_at     TIMESTAMPTZ,
  note          TEXT NOT NULL DEFAULT ''
);
CREATE INDEX resolution_review_pending ON resolution_review (status)
  WHERE status = 'pending';

-- The user's book. Identity belongs to `instrument`; the label and the active
-- flag belong here. There is deliberately no UNIQUE (security): one instrument
-- legitimately wears several security strings over time.
CREATE TABLE book_entry (
  instrument_id  BIGINT PRIMARY KEY REFERENCES instrument(instrument_id),
  asset_class_id BIGINT NOT NULL,
  label          TEXT NOT NULL,
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  added_at       TIMESTAMPTZ NOT NULL DEFAULT now(),
  note           TEXT NOT NULL DEFAULT ''
);
CREATE INDEX book_entry_label_trgm ON book_entry USING gin (label gin_trgm_ops);

-- Every row instrumentListRequest has ever returned, kept forever. This is what
-- makes local search free: one search for "AAPL" seeds all its listings.
CREATE TABLE instrument_candidate (
  id             BIGSERIAL PRIMARY KEY,
  security       TEXT NOT NULL UNIQUE,
  raw_security   TEXT NOT NULL,
  description    TEXT NOT NULL,
  yellow_key     TEXT,
  first_seen     TIMESTAMPTZ NOT NULL DEFAULT now(),
  last_seen      TIMESTAMPTZ NOT NULL DEFAULT now(),
  instrument_id  BIGINT REFERENCES instrument(instrument_id)
);
CREATE INDEX instrument_candidate_sec_trgm
  ON instrument_candidate USING gin (security gin_trgm_ops);
CREATE INDEX instrument_candidate_desc_trgm
  ON instrument_candidate USING gin (description gin_trgm_ops);

-- ---------------------------------------------------------------- pipeline

CREATE TABLE asset_class (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT ''
);

ALTER TABLE book_entry ADD CONSTRAINT book_entry_class_fk
  FOREIGN KEY (asset_class_id) REFERENCES asset_class(id);

-- The configurable field-mapping layer the objectives require. bbg_ftype
-- records P0 5's machine-readable marker: 'BulkFormat' means table-valued.
-- Adding a field stays an INSERT, never a migration.
CREATE TABLE field_def (
  id               BIGSERIAL PRIMARY KEY,
  asset_class_id   BIGINT NOT NULL REFERENCES asset_class(id),
  mnemonic         TEXT NOT NULL,
  label            TEXT NOT NULL,
  value_kind       TEXT NOT NULL CHECK (value_kind IN ('numeric','text','date')),
  bbg_ftype        TEXT,
  bbg_datatype     TEXT,
  entitlement_note TEXT NOT NULL DEFAULT '',
  active           BOOLEAN NOT NULL DEFAULT TRUE,
  UNIQUE (asset_class_id, mnemonic)
);

CREATE TABLE view (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT '',
  active      BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE view_instrument (
  view_id       BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  instrument_id BIGINT NOT NULL REFERENCES instrument(instrument_id),
  PRIMARY KEY (view_id, instrument_id)
);

CREATE TABLE view_field (
  view_id  BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  field_id BIGINT NOT NULL REFERENCES field_def(id),
  PRIMARY KEY (view_id, field_id)
);

-- Amendment A2 status ladder, folded in from migration 0003: data arrives over
-- BLPAPI, so there is no generate stage and reading is not separate from fetching.
CREATE TABLE run (
  id             BIGSERIAL PRIMARY KEY,
  view_id        BIGINT NOT NULL REFERENCES view(id),
  kind           TEXT NOT NULL CHECK (kind IN ('eod','backfill')),
  trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('manual','scheduled')),
  status         TEXT NOT NULL CHECK (status IN
    ('pending','fetching','ingesting','ok','failed','partial')),
  started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at    TIMESTAMPTZ,
  payload_path   TEXT,
  estimated_hits BIGINT NOT NULL DEFAULT 0,
  error_summary  TEXT
);

-- The exact flag combination that produced a value. P0 3 measured that these
-- four flags change the number: AAPL closed 2020-08-28 at 499.23 raw, 124.81
-- split-adjusted, 120.96 fully adjusted. A price without its basis is not a fact.
CREATE TABLE adjustment_basis (
  id              SMALLSERIAL PRIMARY KEY,
  adj_normal      BOOLEAN,
  adj_abnormal    BOOLEAN,
  adj_split       BOOLEAN,
  adj_follow_dpdf BOOLEAN,
  note            TEXT NOT NULL DEFAULT ''
);

INSERT INTO adjustment_basis
  (adj_normal, adj_abnormal, adj_split, adj_follow_dpdf, note) VALUES
  (false, false, false, false,
   'RAW - all four adjustment flags explicitly false. The only combination P0 3.1 measured as unadjusted.'),
  (NULL, NULL, NULL, NULL,
   'LEGACY_DPDF - flags were never set, so the value followed the Terminal''s DPDF<GO> setting, which was not captured. Not reproducible.');

CREATE TABLE observation (
  id             BIGSERIAL PRIMARY KEY,
  instrument_id  BIGINT NOT NULL REFERENCES instrument(instrument_id),
  field_id       BIGINT NOT NULL REFERENCES field_def(id),
  obs_date       DATE NOT NULL,
  obs_time       TIME,
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
  CONSTRAINT observation_one_value
    CHECK ((value_num IS NULL) <> (value_text IS NULL)),
  CONSTRAINT observation_granularity_time
    CHECK ((granularity = 'eod') = (obs_time IS NULL))
);

-- One current row per logical series; the superseded history accumulates beneath.
-- NULLS NOT DISTINCT: obs_time is NULL for every EOD row (see
-- observation_granularity_time) and basis_id is NULL for text-valued fields, so
-- without it Postgres' default NULL-is-distinct behaviour would let unlimited
-- "current" rows through for exactly the series this index exists to protect.
CREATE UNIQUE INDEX observation_current ON observation
  (instrument_id, field_id, obs_date, obs_time, granularity, layer, basis_id)
  NULLS NOT DISTINCT
  WHERE system_to = 'infinity';
CREATE INDEX observation_by_date ON observation (obs_date);

CREATE FUNCTION observation_append_only() RETURNS trigger AS $fn$
BEGIN
  IF NEW.value_num IS DISTINCT FROM OLD.value_num
     OR NEW.value_text IS DISTINCT FROM OLD.value_text
     OR NEW.instrument_id <> OLD.instrument_id
     OR NEW.field_id <> OLD.field_id
     OR NEW.obs_date <> OLD.obs_date
     OR NEW.layer <> OLD.layer
     OR NEW.basis_id IS DISTINCT FROM OLD.basis_id THEN
    RAISE EXCEPTION
      'observations are append-only; close system_to and insert a corrected row';
  END IF;
  RETURN NEW;
END $fn$ LANGUAGE plpgsql;

CREATE TRIGGER observation_append_only BEFORE UPDATE ON observation
  FOR EACH ROW EXECUTE FUNCTION observation_append_only();

CREATE TABLE ingest_issue (
  id            BIGSERIAL PRIMARY KEY,
  run_id        BIGINT NOT NULL REFERENCES run(id),
  instrument_id BIGINT REFERENCES instrument(instrument_id),
  field_id      BIGINT REFERENCES field_def(id),
  obs_date      DATE,
  severity      TEXT NOT NULL CHECK (severity IN ('warn','error')),
  code          TEXT NOT NULL,
  detail        TEXT NOT NULL DEFAULT ''
);

-- run_id is nullable: a Search Bloomberg press is a metered call with no run.
CREATE TABLE hit_ledger (
  id             BIGSERIAL PRIMARY KEY,
  run_id         BIGINT REFERENCES run(id),
  purpose        TEXT NOT NULL DEFAULT 'run',
  estimated_hits BIGINT NOT NULL,
  occurred_on    DATE NOT NULL DEFAULT CURRENT_DATE
);
CREATE INDEX hit_ledger_by_day ON hit_ledger (occurred_on);

CREATE TABLE schedule (
  id           BIGSERIAL PRIMARY KEY,
  view_id      BIGINT NOT NULL REFERENCES view(id),
  active       BOOLEAN NOT NULL DEFAULT TRUE,
  window_start TIME NOT NULL DEFAULT '09:00',
  window_end   TIME NOT NULL DEFAULT '18:00',
  drawn_for    DATE,
  drawn_at     TIME,
  last_result  TEXT,
  CONSTRAINT schedule_view_unique UNIQUE (view_id)
);
