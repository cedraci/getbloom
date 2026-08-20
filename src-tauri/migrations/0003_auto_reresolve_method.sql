-- The post-run dead-security recovery records its decision like every other
-- resolution path (an audit trail with holes where the automatic answers
-- went cannot answer "why is this instrument bound to that security"), so
-- its method joins the domain.
ALTER TABLE resolution_decision
  DROP CONSTRAINT resolution_decision_method_check;
ALTER TABLE resolution_decision
  ADD CONSTRAINT resolution_decision_method_check CHECK (method IN
    ('local_alias','bloomberg_ref','bloomberg_list','manual','auto_reresolve'));
