-- Add token budget fields to clients.
-- NULL token_budget means unlimited (no enforcement).
ALTER TABLE clients
    ADD COLUMN token_budget     BIGINT,
    ADD COLUMN budget_period    TEXT,
    ADD COLUMN budget_reset_at  TIMESTAMPTZ;
