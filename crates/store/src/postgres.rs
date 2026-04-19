use async_trait::async_trait;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use tracing::{debug, info};
use uuid::Uuid;

use sre_common::errors::StoreError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::AgentMessage;

use crate::models::{WorkflowRecord, WorkflowState};
use crate::traits::{EventLog, WorkflowStore};

/// Postgres-backed implementation of WorkflowStore and EventLog
pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Create a new PgStore, running migrations
    pub async fn new(database_url: &str) -> Result<Self, StoreError> {
        let options = PgConnectOptions::from_str(database_url)
            .map_err(|e| StoreError::Database(e.to_string()))?;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|e| StoreError::Database(e.to_string()))?;

        let store = Self { pool };
        store.run_migrations().await?;
        info!("Postgres store initialized");
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
        sqlx::migrate!()
            .run(&self.pool)
            .await
            .map_err(|e| StoreError::MigrationFailed(e.to_string()))?;

        debug!("Postgres migrations complete");
        Ok(())
    }
}

#[async_trait]
impl WorkflowStore for PgStore {
    #[tracing::instrument(skip(self, workflow), fields(workflow_id = %workflow.id))]
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
            VALUES ($1, $2, $3, $4, $5, $6, $7)
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

    #[tracing::instrument(skip(self), fields(workflow_id = %workflow_id, new_state = ?state))]
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
            UPDATE workflows SET state = $1, updated_at = $2 WHERE id = $3
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
            FROM workflows WHERE id = $1
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

    #[tracing::instrument(skip(self, report), fields(workflow_id = %workflow_id))]
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
            UPDATE workflows SET report_json = $1 WHERE id = $2
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
impl EventLog for PgStore {
    #[tracing::instrument(skip(self, message), fields(correlation_id = %message.correlation_id))]
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
            VALUES ($1, $2, $3, $4, $5, $6, $7)
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
            WHERE correlation_id = $1
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
            SELECT COUNT(*) FROM event_log WHERE correlation_id = $1
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
