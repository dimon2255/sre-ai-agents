use async_trait::async_trait;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use std::str::FromStr;
use tracing::{debug, info};
use uuid::Uuid;

use sre_common::errors::StoreError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::AgentMessage;

use crate::models::{WorkflowRecord, WorkflowState};
use crate::traits::{EventLog, WorkflowStore};

/// SQLite-backed implementation of WorkflowStore and EventLog
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Create a new SqliteStore, running migrations
    pub async fn new(database_url: &str) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str(database_url)
            .map_err(|e| StoreError::Database(e.to_string()))?
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        let store = Self { pool };
        store.run_migrations().await?;
        info!("SQLite store initialized");
        Ok(store)
    }

    /// Lightweight connectivity check for readiness probes
    pub async fn ping(&self) -> Result<(), StoreError> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;
        Ok(())
    }

    async fn run_migrations(&self) -> Result<(), StoreError> {
        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS workflows (
                id TEXT PRIMARY KEY,
                state TEXT NOT NULL,
                alert_id TEXT NOT NULL,
                affected_resource TEXT NOT NULL,
                severity TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL,
                report_json TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::MigrationFailed(e.to_string()))?;

        sqlx::query(
            r#"
            CREATE TABLE IF NOT EXISTS event_log (
                id TEXT PRIMARY KEY,
                correlation_id TEXT NOT NULL,
                source_agent TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                confidence REAL NOT NULL,
                timestamp TEXT NOT NULL,
                parent_message_id TEXT
            )
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::MigrationFailed(e.to_string()))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_event_log_correlation
            ON event_log(correlation_id)
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::MigrationFailed(e.to_string()))?;

        sqlx::query(
            r#"
            CREATE INDEX IF NOT EXISTS idx_workflows_state
            ON workflows(state)
            "#,
        )
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::MigrationFailed(e.to_string()))?;

        debug!("SQLite migrations complete");
        Ok(())
    }
}

#[async_trait]
impl WorkflowStore for SqliteStore {
    async fn create_workflow(&self, workflow: &WorkflowRecord) -> Result<(), StoreError> {
        let id = workflow.id.to_string();
        let state = serde_json::to_string(&workflow.state)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        let severity = serde_json::to_string(&workflow.severity)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        let created_at = workflow.created_at.to_rfc3339();
        let updated_at = workflow.updated_at.to_rfc3339();

        sqlx::query(
            r#"
            INSERT INTO workflows (id, state, alert_id, affected_resource, severity, created_at, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(&state)
        .bind(&workflow.alert_id)
        .bind(&workflow.affected_resource)
        .bind(&severity)
        .bind(&created_at)
        .bind(&updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        debug!(workflow_id = %id, "Workflow created");
        Ok(())
    }

    async fn update_state(
        &self,
        workflow_id: &Uuid,
        state: WorkflowState,
    ) -> Result<(), StoreError> {
        let id = workflow_id.to_string();
        let state_json =
            serde_json::to_string(&state).map_err(|e| StoreError::Serialization(e.to_string()))?;
        let now = chrono::Utc::now().to_rfc3339();

        let result = sqlx::query(
            r#"
            UPDATE workflows SET state = ?, updated_at = ? WHERE id = ?
            "#,
        )
        .bind(&state_json)
        .bind(&now)
        .bind(&id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(StoreError::WorkflowNotFound(id));
        }

        debug!(workflow_id = %id, new_state = %state, "Workflow state updated");
        Ok(())
    }

    async fn get_workflow(&self, workflow_id: &Uuid) -> Result<Option<WorkflowRecord>, StoreError> {
        let id = workflow_id.to_string();

        let row = sqlx::query_as::<_, WorkflowRow>(
            r#"
            SELECT id, state, alert_id, affected_resource, severity, created_at, updated_at, report_json
            FROM workflows WHERE id = ?
            "#,
        )
        .bind(&id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        row.map(|r| r.into_record()).transpose()
    }

    async fn get_active_workflows(&self) -> Result<Vec<WorkflowRecord>, StoreError> {
        let rows = sqlx::query_as::<_, WorkflowRow>(
            r#"
            SELECT id, state, alert_id, affected_resource, severity, created_at, updated_at, report_json
            FROM workflows
            WHERE state NOT IN ('"resolved"', '"escalated"', '"failed"')
            "#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        rows.into_iter().map(|r| r.into_record()).collect()
    }

    async fn store_report(
        &self,
        workflow_id: &Uuid,
        report: &InvestigationReport,
    ) -> Result<(), StoreError> {
        let id = workflow_id.to_string();
        let report_json =
            serde_json::to_string(report).map_err(|e| StoreError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            UPDATE workflows SET report_json = ? WHERE id = ?
            "#,
        )
        .bind(&report_json)
        .bind(&id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        debug!(workflow_id = %id, "Report stored");
        Ok(())
    }
}

#[async_trait]
impl EventLog for SqliteStore {
    async fn append(&self, message: &AgentMessage) -> Result<(), StoreError> {
        let id = message.id.to_string();
        let correlation_id = message.correlation_id.to_string();
        let source_agent = message.source_agent.to_string();
        let payload_json = serde_json::to_string(&message)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        let timestamp = message.timestamp.to_rfc3339();
        let parent_id = message.parent_message_id.map(|id| id.to_string());

        sqlx::query(
            r#"
            INSERT INTO event_log (id, correlation_id, source_agent, payload_json, confidence, timestamp, parent_message_id)
            VALUES (?, ?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&id)
        .bind(&correlation_id)
        .bind(&source_agent)
        .bind(&payload_json)
        .bind(message.confidence)
        .bind(&timestamp)
        .bind(&parent_id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    async fn get_by_correlation_id(
        &self,
        correlation_id: &Uuid,
    ) -> Result<Vec<AgentMessage>, StoreError> {
        let cid = correlation_id.to_string();

        let rows = sqlx::query_as::<_, EventRow>(
            r#"
            SELECT payload_json FROM event_log
            WHERE correlation_id = ?
            ORDER BY timestamp ASC
            "#,
        )
        .bind(&cid)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        rows.iter()
            .map(|r| {
                serde_json::from_str(&r.payload_json)
                    .map_err(|e| StoreError::Serialization(e.to_string()))
            })
            .collect()
    }

    async fn count_by_correlation_id(&self, correlation_id: &Uuid) -> Result<u64, StoreError> {
        let cid = correlation_id.to_string();

        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM event_log WHERE correlation_id = ?
            "#,
        )
        .bind(&cid)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(row.0 as u64)
    }
}

// Internal row types for sqlx mapping

#[derive(sqlx::FromRow)]
struct WorkflowRow {
    id: String,
    state: String,
    alert_id: String,
    affected_resource: String,
    severity: String,
    created_at: String,
    updated_at: String,
    report_json: Option<String>,
}

impl WorkflowRow {
    fn into_record(self) -> Result<WorkflowRecord, StoreError> {
        Ok(WorkflowRecord {
            id: Uuid::parse_str(&self.id).map_err(|e| StoreError::Serialization(e.to_string()))?,
            state: serde_json::from_str(&self.state)
                .map_err(|e| StoreError::Serialization(e.to_string()))?,
            alert_id: self.alert_id,
            affected_resource: self.affected_resource,
            severity: serde_json::from_str(&self.severity)
                .map_err(|e| StoreError::Serialization(e.to_string()))?,
            created_at: chrono::DateTime::parse_from_rfc3339(&self.created_at)
                .map_err(|e| StoreError::Serialization(e.to_string()))?
                .with_timezone(&chrono::Utc),
            updated_at: chrono::DateTime::parse_from_rfc3339(&self.updated_at)
                .map_err(|e| StoreError::Serialization(e.to_string()))?
                .with_timezone(&chrono::Utc),
            report_json: self.report_json,
        })
    }
}

#[derive(sqlx::FromRow)]
struct EventRow {
    payload_json: String,
}
