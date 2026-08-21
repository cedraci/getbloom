-- P8: an instrument resolved without a LISTING_DATE starts its currency
-- belief at its add date, which left every earlier observation -- the first
-- EOD run fetches YESTERDAY, and backfills reach further -- with an unknown
-- currency. The earliest belief extends backward: knowing it trades in USD
-- today is the best statement we have about last week too. Ingest now stamps
-- this way; this restamps the rows written NULL before the fix.
ALTER TABLE observation DISABLE TRIGGER observation_append_only;
UPDATE observation o SET currency = e.value
  FROM (SELECT DISTINCT ON (instrument_id) instrument_id, value, valid_from
          FROM instrument_attr
         WHERE attr = 'currency' AND system_to = 'infinity'
         ORDER BY instrument_id, valid_from) e
 WHERE e.instrument_id = o.instrument_id
   AND o.currency IS NULL
   AND o.value_num IS NOT NULL
   AND o.obs_date < e.valid_from;
ALTER TABLE observation ENABLE TRIGGER observation_append_only;
