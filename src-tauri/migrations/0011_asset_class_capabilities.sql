-- P9: per-asset-class capability flags. Defaults keep every existing class
-- equity-shaped (corp actions on, M&A lifecycle on, factor adjustment, no
-- class staleness default), so no data migration is needed.
ALTER TABLE asset_class
  ADD COLUMN corp_actions_capable BOOLEAN NOT NULL DEFAULT TRUE,
  ADD COLUMN ma_capable           BOOLEAN NOT NULL DEFAULT TRUE,
  ADD COLUMN adjustment_style     TEXT NOT NULL DEFAULT 'factors'
      CONSTRAINT asset_class_adjustment_style_check
      CHECK (adjustment_style IN ('factors', 'none')),
  ADD COLUMN qc_stale_days_default INTEGER
      CONSTRAINT asset_class_qc_stale_default_min
      CHECK (qc_stale_days_default IS NULL OR qc_stale_days_default >= 2);
