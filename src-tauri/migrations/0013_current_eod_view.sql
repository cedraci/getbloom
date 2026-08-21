-- P10: the one query downstream consumers need, without understanding
-- bitemporality. Current belief, raw layer, EOD granularity. Adjusted and
-- stitched series stay app-level (they are mode-parameterised).
CREATE VIEW current_eod AS
SELECT o.instrument_id,
       be.label,
       f.mnemonic,
       o.obs_date,
       o.value_num,
       o.value_text,
       o.currency,
       o.run_id,
       o.system_from AS believed_since
FROM observation o
JOIN field_def f  ON f.id = o.field_id
LEFT JOIN book_entry be ON be.instrument_id = o.instrument_id
WHERE o.system_to = 'infinity'
  AND o.layer = 'raw'
  AND o.granularity = 'eod';
