-- Repair assets created before resolve_bdp_security() learned to detect a yellow
-- key already present on the identifier.
--
-- Typing the full Bloomberg name "AAPL US Equity" into the ticker box, with the
-- yellow-key box saying "Equity", produced bdp_security "AAPL US Equity Equity".
-- Every request for such an asset came back BAD_SEC / INVALID_SECURITY.
--
-- asset has UNIQUE (bdp_security), and the user may well have re-added the same
-- security correctly after the broken one failed. Repairing blindly would then
-- collide and abort the migration -- which runs at startup, so a failure here
-- stops the app from booting. Every statement below is therefore conditional on
-- the repaired value being free.

-- 1. Repair the ones whose corrected security is not already taken.
UPDATE asset a
SET bdp_security = rtrim(left(a.bdp_security, length(a.bdp_security) - length(a.yellow_key))),
    ticker = CASE
      WHEN a.ticker IS NOT NULL
       AND lower(a.ticker) LIKE lower('% ' || a.yellow_key)
       AND lower(a.ticker) <> lower(a.yellow_key)
      THEN rtrim(left(a.ticker, length(a.ticker) - length(a.yellow_key)))
      ELSE a.ticker END
WHERE lower(a.bdp_security) LIKE lower('% ' || a.yellow_key || ' ' || a.yellow_key)
  AND NOT EXISTS (
    SELECT 1 FROM asset b
    WHERE b.id <> a.id
      AND b.bdp_security =
          rtrim(left(a.bdp_security, length(a.bdp_security) - length(a.yellow_key))));

-- 2. Whatever still carries a doubled key is a duplicate of a row that is already
--    correct. Deactivate rather than delete: the data is the user's, the row is
--    still visible in the UI, and the checkbox re-enables it.
UPDATE asset
SET active = false
WHERE active
  AND lower(bdp_security) LIKE lower('% ' || yellow_key || ' ' || yellow_key);
