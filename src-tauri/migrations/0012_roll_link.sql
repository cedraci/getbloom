-- P9: futures roll junctions splice by DIFFERENCE, not ratio. A roll link
-- carries a signed offset: successor = predecessor + roll_offset at the
-- junction. exchange_ratio CHECKs (> 0), so it cannot hold this.
ALTER TABLE instrument_link DROP CONSTRAINT instrument_link_link_type_check;
ALTER TABLE instrument_link ADD CONSTRAINT instrument_link_link_type_check
  CHECK (link_type IN ('rename', 'merger', 'conversion', 'share_class_change',
                       'spinoff', 'roll'));
ALTER TABLE instrument_link ADD COLUMN roll_offset DOUBLE PRECISION
  CONSTRAINT instrument_link_roll_offset_roll_only
  CHECK (roll_offset IS NULL OR link_type = 'roll');
