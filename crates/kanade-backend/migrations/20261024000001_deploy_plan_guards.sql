-- Keep immutable authorization bindings protected even from direct SQL edits.
CREATE TRIGGER deploy_plan_immutable BEFORE UPDATE ON deploy_plans
WHEN NEW.plan_id IS NOT OLD.plan_id OR NEW.record_version IS NOT OLD.record_version
 OR NEW.plan_digest IS NOT OLD.plan_digest OR NEW.content IS NOT OLD.content
 OR NEW.job_manifest IS NOT OLD.job_manifest OR NEW.job_id IS NOT OLD.job_id
 OR NEW.role IS NOT OLD.role OR NEW.not_before IS NOT OLD.not_before
 OR NEW.expires_at IS NOT OLD.expires_at OR NEW.created_by IS NOT OLD.created_by
 OR NEW.created_at IS NOT OLD.created_at
BEGIN SELECT RAISE(ABORT, 'deployment plan contents are immutable'); END;

-- The approved -> approved revision step is the existing consume transaction's
-- write lock; its single-use ledger is committed atomically with that step.
CREATE TRIGGER deploy_plan_transition BEFORE UPDATE ON deploy_plans
WHEN NEW.revision != OLD.revision + 1
 OR NOT ((OLD.state = 'pending' AND NEW.state IN ('approved', 'revoked'))
      OR (OLD.state = 'approved' AND NEW.state IN ('approved', 'revoked')))
 OR (OLD.state = 'pending' AND NEW.state = 'revoked' AND NEW.approved_by IS NOT NULL)
 OR (OLD.approved_by IS NOT NULL AND
     (NEW.approved_by IS NOT OLD.approved_by OR NEW.approved_at IS NOT OLD.approved_at))
BEGIN SELECT RAISE(ABORT, 'invalid deployment plan transition'); END;

-- The running architecture and the architecture in the capability snapshot
-- are independent of handler declarations. NULL remains unknown for old agents.
ALTER TABLE agents ADD COLUMN agent_arch TEXT;
ALTER TABLE agents ADD COLUMN deploy_evidence_agent_arch TEXT;
