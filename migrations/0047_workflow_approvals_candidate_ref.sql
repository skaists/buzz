-- WF-08: bind a pending approval to the exact candidate it was requested for.
-- Nullable and additive: gates without a candidate keep NULL and behave as before.
SET LOCAL lock_timeout = '5s';

ALTER TABLE workflow_approvals ADD COLUMN candidate_ref TEXT;
