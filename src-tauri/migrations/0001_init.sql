-- TimescaleDB is used when present but is NOT required: nothing in this schema
-- depends on a hypertable feature (no compression, retention, or continuous
-- aggregates), and Timescale ships no Windows builds. Without it `observation`
-- is a plain table with identical keys and behaviour.
DO $$
BEGIN
    EXECUTE 'CREATE EXTENSION IF NOT EXISTS timescaledb';
EXCEPTION WHEN OTHERS THEN
    RAISE NOTICE 'TimescaleDB unavailable (%) - continuing without it', SQLERRM;
END $$;

CREATE TABLE asset_class (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT ''
);

CREATE TABLE asset (
  id             BIGSERIAL PRIMARY KEY,
  asset_class_id BIGINT NOT NULL REFERENCES asset_class(id),
  label          TEXT NOT NULL,
  id_kind        TEXT NOT NULL CHECK (id_kind IN ('ticker','isin')),
  ticker         TEXT,
  isin           TEXT,
  yellow_key     TEXT NOT NULL,
  bdp_security   TEXT NOT NULL,
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  CHECK ((id_kind = 'ticker' AND ticker IS NOT NULL)
      OR (id_kind = 'isin'   AND isin   IS NOT NULL)),
  UNIQUE (bdp_security)
);

CREATE TABLE field_def (
  id             BIGSERIAL PRIMARY KEY,
  asset_class_id BIGINT NOT NULL REFERENCES asset_class(id),
  mnemonic       TEXT NOT NULL,
  label          TEXT NOT NULL,
  value_kind     TEXT NOT NULL CHECK (value_kind IN ('numeric','text','date')),
  active         BOOLEAN NOT NULL DEFAULT TRUE,
  UNIQUE (asset_class_id, mnemonic)
);

CREATE TABLE view (
  id          BIGSERIAL PRIMARY KEY,
  name        TEXT NOT NULL UNIQUE,
  description TEXT NOT NULL DEFAULT '',
  active      BOOLEAN NOT NULL DEFAULT TRUE
);

CREATE TABLE view_asset (
  view_id  BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  asset_id BIGINT NOT NULL REFERENCES asset(id),
  PRIMARY KEY (view_id, asset_id)
);

CREATE TABLE view_field (
  view_id  BIGINT NOT NULL REFERENCES view(id) ON DELETE CASCADE,
  field_id BIGINT NOT NULL REFERENCES field_def(id),
  PRIMARY KEY (view_id, field_id)
);

CREATE TABLE run (
  id             BIGSERIAL PRIMARY KEY,
  view_id        BIGINT NOT NULL REFERENCES view(id),
  kind           TEXT NOT NULL CHECK (kind IN ('eod','backfill')),
  trigger_kind   TEXT NOT NULL CHECK (trigger_kind IN ('manual','scheduled')),
  status         TEXT NOT NULL CHECK (status IN
    ('generating','refreshing','reading','ingesting','ok','failed','partial')),
  started_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
  finished_at    TIMESTAMPTZ,
  workbook_path  TEXT,
  estimated_hits BIGINT NOT NULL DEFAULT 0,
  error_summary  TEXT
);

CREATE TABLE observation (
  asset_id    BIGINT NOT NULL REFERENCES asset(id),
  field_id    BIGINT NOT NULL REFERENCES field_def(id),
  obs_date    DATE   NOT NULL,
  value_num   DOUBLE PRECISION,
  value_text  TEXT,
  run_id      BIGINT NOT NULL REFERENCES run(id),
  ingested_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (asset_id, field_id, obs_date),
  CHECK ((value_num IS NULL) <> (value_text IS NULL))
);
DO $$
BEGIN
    IF EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'timescaledb') THEN
        PERFORM create_hypertable('observation', 'obs_date');
    END IF;
END $$;

CREATE TABLE ingest_issue (
  id       BIGSERIAL PRIMARY KEY,
  run_id   BIGINT NOT NULL REFERENCES run(id),
  asset_id BIGINT REFERENCES asset(id),
  field_id BIGINT REFERENCES field_def(id),
  obs_date DATE,
  severity TEXT NOT NULL CHECK (severity IN ('warn','error')),
  code     TEXT NOT NULL,
  detail   TEXT NOT NULL DEFAULT ''
);

CREATE TABLE hit_ledger (
  id             BIGSERIAL PRIMARY KEY,
  run_id         BIGINT NOT NULL REFERENCES run(id),
  estimated_hits BIGINT NOT NULL,
  occurred_on    DATE NOT NULL DEFAULT CURRENT_DATE
);

CREATE TABLE schedule (
  id           BIGSERIAL PRIMARY KEY,
  view_id      BIGINT NOT NULL REFERENCES view(id),
  active       BOOLEAN NOT NULL DEFAULT TRUE,
  window_start TIME NOT NULL DEFAULT '09:00',
  window_end   TIME NOT NULL DEFAULT '18:00',
  drawn_for    DATE,
  drawn_at     TIME,
  last_result  TEXT
);
