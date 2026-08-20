-- Evidence that Bloomberg answered "Field not applicable to security" for
-- BOTH corp-action fields (live 2026-08-21: YODA LN Equity, an accumulating
-- ETF with no distributions to report). The automatic per-run refresh skips
-- these instruments so their 2 hits are not re-charged every day; a manual
-- refresh retries anyway and, on success, deletes the row.
CREATE TABLE corp_actions_na (
  instrument_id BIGINT PRIMARY KEY REFERENCES instrument(instrument_id),
  noted_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
  detail        TEXT NOT NULL
);
