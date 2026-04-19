CREATE TABLE IF NOT EXISTS workflows (
    id TEXT PRIMARY KEY,
    state TEXT NOT NULL,
    alert_id TEXT NOT NULL,
    affected_resource TEXT NOT NULL,
    severity TEXT NOT NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    report_json TEXT
);

CREATE TABLE IF NOT EXISTS event_log (
    id TEXT PRIMARY KEY,
    correlation_id TEXT NOT NULL,
    source_agent TEXT NOT NULL,
    payload_json TEXT NOT NULL,
    confidence REAL NOT NULL,
    timestamp TEXT NOT NULL,
    parent_message_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_event_log_correlation
    ON event_log (correlation_id);

CREATE INDEX IF NOT EXISTS idx_workflows_state
    ON workflows (state);
