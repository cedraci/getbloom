-- P11: publication cadence + fetch capability + the identity-sweep schedule
-- columns (design 11.1, 11.2, 11.8). Defaults keep every existing class and
-- field daily/history/no-sweep-shaped -- bit-for-bit today's behaviour.
-- Migration stance (user ruling 2026-08-22): DB contents are disposable in
-- this test environment, so defaults are chosen freely; no data migration.

-- 11.1 Cadence model: class default + per-field override. Effective cadence
-- everywhere = COALESCE(field_def.cadence, asset_class.default_cadence) --
-- the same COALESCE idiom as qc_stale_days (P7/P9).
ALTER TABLE asset_class
  ADD COLUMN default_cadence TEXT NOT NULL DEFAULT 'daily'
      CONSTRAINT asset_class_default_cadence_check
      CHECK (default_cadence IN ('daily','weekly','monthly','quarterly','irregular')),
  ADD COLUMN cadence_grace_days INTEGER NOT NULL DEFAULT 10
      CONSTRAINT asset_class_cadence_grace_days_check
      CHECK (cadence_grace_days >= 0);

ALTER TABLE field_def
  ADD COLUMN cadence TEXT
      CONSTRAINT field_def_cadence_check
      CHECK (cadence IS NULL OR cadence IN ('daily','weekly','monthly','quarterly','irregular'));

-- 11.2 Fetch capability: which wire path collects this field.
ALTER TABLE field_def
  ADD COLUMN fetch_via TEXT NOT NULL DEFAULT 'history'
      CONSTRAINT field_def_fetch_via_check
      CHECK (fetch_via IN ('history','reference'));

-- 11.8 Identity sweep class capability. Default 'none': FX/spot/generic-future/
-- index classes never had a retirement trigger and must not gain one silently
-- -- equity/fund classes opt in via Settings (no existing class is flipped by
-- this migration).
ALTER TABLE asset_class
  ADD COLUMN identity_sweep TEXT NOT NULL DEFAULT 'none'
      CONSTRAINT asset_class_identity_sweep_check
      CHECK (identity_sweep IN ('none','market_status','maturity'));

-- Controller ruling R1: the weekly identity sweep rides the existing
-- verify-day slot machinery, mirroring verify_dow/last_verified_on
-- (0007_quality_and_verify.sql) exactly. NULL/no default = off; Task 6 wires
-- the scheduler logic that reads these -- this migration only adds the
-- columns.
ALTER TABLE schedule ADD COLUMN identity_dow SMALLINT
  CONSTRAINT schedule_identity_dow_range
  CHECK (identity_dow IS NULL OR identity_dow BETWEEN 1 AND 7);
ALTER TABLE schedule ADD COLUMN last_identity_on DATE;
