-- ADR-013 cross-replica dedup. The Phase 2a/2b ingestion paths (webhook +
-- NATS) can both surface the same alert_id, and with multiple ACA replicas
-- the in-memory dedup LRU is not authoritative. This migration makes
-- `workflows.alert_id` UNIQUE so the database is the cross-process gate;
-- the orchestrator uses `INSERT … ON CONFLICT (alert_id) DO NOTHING` to
-- claim each alert exactly once.
--
-- Pre-existing duplicates are collapsed first: for each alert_id with more
-- than one row, we keep the row with the earliest created_at (the original
-- investigation; any later rows are the duplicates this constraint exists
-- to prevent). The deletion runs in a CTE so the constraint creation that
-- follows has a clean dataset.
--
-- Safe to re-run during local dev because `ADD CONSTRAINT IF NOT EXISTS`
-- is not supported on Postgres < 17; the equivalent is done via a guarded
-- DO block.

WITH ranked AS (
    SELECT
        id,
        alert_id,
        ROW_NUMBER() OVER (
            PARTITION BY alert_id
            ORDER BY created_at ASC, id ASC
        ) AS rn
    FROM workflows
)
DELETE FROM workflows w
USING ranked r
WHERE w.id = r.id
  AND r.rn > 1;

DO $$
BEGIN
    IF NOT EXISTS (
        SELECT 1
        FROM pg_constraint
        WHERE conname = 'workflows_alert_id_unique'
    ) THEN
        ALTER TABLE workflows
            ADD CONSTRAINT workflows_alert_id_unique UNIQUE (alert_id);
    END IF;
END $$;
