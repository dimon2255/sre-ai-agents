-- ADR-012 corrective migration. See docs/adr/012-schema-type-modernization.md.
--
-- The Phase 2a init migration (20260418000001_init.sql) ported the SQLite
-- schema verbatim — every column was TEXT or REAL. This migration converts
-- columns to their proper Postgres-native types and introduces ENUMs for
-- state/severity so filters don't have to match JSON-quoted string literals.
--
-- Safe to run against the populated staging DB: ALTER TABLE ... USING
-- performs a full table rewrite, which is cheap at our current volume.

-- ─── ENUM types ───

CREATE TYPE workflow_state AS ENUM (
    'created',
    'triaging',
    'investigating',
    'synthesizing',
    'resolved',
    'escalated',
    'failed'
);

CREATE TYPE alert_severity AS ENUM (
    'Sev0',
    'Sev1',
    'Sev2',
    'Sev3',
    'Sev4'
);

-- ─── workflows table ───

-- Note on USING clauses: existing rows store `state` and `severity` as
-- JSON-encoded strings (e.g. the literal seven characters `"escalated"`).
-- `trim(both '"' from col)` strips the embedded quotes before the enum cast.

ALTER TABLE workflows
    ALTER COLUMN id          TYPE uuid            USING id::uuid,
    ALTER COLUMN state       TYPE workflow_state  USING trim(both '"' from state)::workflow_state,
    ALTER COLUMN severity    TYPE alert_severity  USING trim(both '"' from severity)::alert_severity,
    ALTER COLUMN created_at  TYPE timestamptz     USING created_at::timestamptz,
    ALTER COLUMN updated_at  TYPE timestamptz     USING updated_at::timestamptz,
    ALTER COLUMN report_json TYPE jsonb           USING report_json::jsonb;

-- ─── event_log table ───

ALTER TABLE event_log
    ALTER COLUMN id                TYPE uuid             USING id::uuid,
    ALTER COLUMN correlation_id    TYPE uuid             USING correlation_id::uuid,
    ALTER COLUMN parent_message_id TYPE uuid             USING parent_message_id::uuid,
    ALTER COLUMN timestamp         TYPE timestamptz      USING timestamp::timestamptz,
    ALTER COLUMN payload_json      TYPE jsonb            USING payload_json::jsonb,
    ALTER COLUMN confidence        TYPE double precision USING confidence::double precision;

-- ─── Indexes ───
--
-- `get_by_correlation_id` filters by correlation_id and sorts by timestamp.
-- A composite index serves that with an index-ordered scan; the old
-- single-column index required an extra sort step.

DROP INDEX IF EXISTS idx_event_log_correlation;

CREATE INDEX IF NOT EXISTS idx_event_log_correlation_timestamp
    ON event_log (correlation_id, timestamp);
