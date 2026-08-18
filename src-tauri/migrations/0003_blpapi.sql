-- Amendment A2: data is fetched over BLPAPI, not through an Excel workbook.
--
-- The audit trail is now the sidecar's raw JSON response rather than an
-- .xlsx, and the status ladder collapses: there is no generate stage and
-- reading is no longer a separate phase from fetching.
ALTER TABLE run RENAME COLUMN workbook_path TO payload_path;

UPDATE run SET status = 'fetching'
 WHERE status IN ('generating', 'refreshing', 'reading');

ALTER TABLE run DROP CONSTRAINT IF EXISTS run_status_check;
ALTER TABLE run ADD CONSTRAINT run_status_check CHECK (status IN
  ('pending', 'fetching', 'ingesting', 'ok', 'failed', 'partial'));
