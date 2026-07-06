use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};
use std::str::FromStr;
use std::time::Duration;
use tracing::{debug, info};
use uuid::Uuid;

use sre_common::errors::StoreError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::{AgentMessage, AlertSeverity};

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
            .map_err(|e| StoreError::Database(e.to_string()))?
            // Server-side safety net: a query stuck behind a Postgres failover
            // must not pin a pool connection forever (the readiness probe shares
            // this pool). statement_timeout caps any single statement; the
            // idle-in-transaction timeout releases row locks held by a wedged txn.
            .options([
                ("statement_timeout", "15000"),
                ("idle_in_transaction_session_timeout", "30000"),
            ]);

        let pool = PgPoolOptions::new()
            // Sized above orchestrator.max_concurrent_investigations (default 10)
            // so concurrent investigation writes never starve the readiness ping.
            .max_connections(16)
            .acquire_timeout(Duration::from_secs(10))
            .max_lifetime(Duration::from_secs(30 * 60))
            .idle_timeout(Duration::from_secs(10 * 60))
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
        sqlx::query(
            r#"
            INSERT INTO workflows (id, state, alert_id, affected_resource, severity, created_at, updated_at)
            VALUES ($1, $2::workflow_state, $3, $4, $5::alert_severity, $6, $7)
            "#,
        )
        .bind(workflow.id)
        .bind(workflow.state.to_string())
        .bind(&workflow.alert_id)
        .bind(&workflow.affected_resource)
        .bind(workflow.severity.as_label())
        .bind(workflow.created_at)
        .bind(workflow.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        debug!(workflow_id = %workflow.id, "Workflow created");
        Ok(())
    }

    #[tracing::instrument(
        skip(self, workflow),
        fields(workflow_id = %workflow.id, alert_id = %workflow.alert_id)
    )]
    async fn try_create_workflow(&self, workflow: &WorkflowRecord) -> Result<bool, StoreError> {
        let result = sqlx::query(
            r#"
            INSERT INTO workflows (id, state, alert_id, affected_resource, severity, created_at, updated_at)
            VALUES ($1, $2::workflow_state, $3, $4, $5::alert_severity, $6, $7)
            ON CONFLICT (alert_id) DO NOTHING
            "#,
        )
        .bind(workflow.id)
        .bind(workflow.state.to_string())
        .bind(&workflow.alert_id)
        .bind(&workflow.affected_resource)
        .bind(workflow.severity.as_label())
        .bind(workflow.created_at)
        .bind(workflow.updated_at)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        let inserted = result.rows_affected() == 1;
        debug!(workflow_id = %workflow.id, inserted, "try_create_workflow");
        Ok(inserted)
    }

    #[tracing::instrument(skip(self), fields(workflow_id = %workflow_id, new_state = ?state))]
    async fn update_state(
        &self,
        workflow_id: &Uuid,
        state: WorkflowState,
    ) -> Result<(), StoreError> {
        let result = sqlx::query(
            r#"
            UPDATE workflows SET state = $1::workflow_state, updated_at = $2 WHERE id = $3
            "#,
        )
        .bind(state.to_string())
        .bind(Utc::now())
        .bind(workflow_id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        if result.rows_affected() == 0 {
            return Err(StoreError::WorkflowNotFound(workflow_id.to_string()));
        }

        debug!(workflow_id = %workflow_id, new_state = %state, "Workflow state updated");
        Ok(())
    }

    async fn get_workflow(&self, workflow_id: &Uuid) -> Result<Option<WorkflowRecord>, StoreError> {
        let row = sqlx::query_as::<_, WorkflowRow>(
            r#"
            SELECT id, state::text AS state, alert_id, affected_resource,
                   severity::text AS severity, created_at, updated_at, report_json
            FROM workflows WHERE id = $1
            "#,
        )
        .bind(workflow_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        row.map(|r| r.into_record()).transpose()
    }

    async fn get_workflow_by_alert_id(
        &self,
        alert_id: &str,
    ) -> Result<Option<WorkflowRecord>, StoreError> {
        let row = sqlx::query_as::<_, WorkflowRow>(
            r#"
            SELECT id, state::text AS state, alert_id, affected_resource,
                   severity::text AS severity, created_at, updated_at, report_json
            FROM workflows WHERE alert_id = $1
            ORDER BY created_at DESC LIMIT 1
            "#,
        )
        .bind(alert_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        row.map(|r| r.into_record()).transpose()
    }

    async fn get_active_workflows(&self) -> Result<Vec<WorkflowRecord>, StoreError> {
        let rows = sqlx::query_as::<_, WorkflowRow>(
            r#"
            SELECT id, state::text AS state, alert_id, affected_resource,
                   severity::text AS severity, created_at, updated_at, report_json
            FROM workflows
            WHERE state NOT IN ('resolved', 'escalated', 'failed')
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
        let report_json =
            serde_json::to_string(report).map_err(|e| StoreError::Serialization(e.to_string()))?;

        sqlx::query(
            r#"
            UPDATE workflows SET report_json = $1::jsonb WHERE id = $2
            "#,
        )
        .bind(&report_json)
        .bind(workflow_id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        debug!(workflow_id = %workflow_id, "Report stored");
        Ok(())
    }
}

#[async_trait]
impl EventLog for PgStore {
    #[tracing::instrument(skip(self, message), fields(correlation_id = %message.correlation_id))]
    async fn append(&self, message: &AgentMessage) -> Result<(), StoreError> {
        let payload_json = serde_json::to_string(&message)
            .map_err(|e| StoreError::Serialization(e.to_string()))?;
        let source_agent = message.source_agent.to_string();

        sqlx::query(
            r#"
            INSERT INTO event_log (id, correlation_id, source_agent, payload_json, confidence, timestamp, parent_message_id)
            VALUES ($1, $2, $3, $4::jsonb, $5, $6, $7)
            "#,
        )
        .bind(message.id)
        .bind(message.correlation_id)
        .bind(&source_agent)
        .bind(&payload_json)
        .bind(message.confidence)
        .bind(message.timestamp)
        .bind(message.parent_message_id)
        .execute(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(())
    }

    async fn get_by_correlation_id(
        &self,
        correlation_id: &Uuid,
    ) -> Result<Vec<AgentMessage>, StoreError> {
        let rows = sqlx::query_as::<_, EventRow>(
            r#"
            SELECT payload_json FROM event_log
            WHERE correlation_id = $1
            ORDER BY timestamp ASC
            "#,
        )
        .bind(correlation_id)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        rows.into_iter()
            .map(|r| {
                serde_json::from_value(r.payload_json)
                    .map_err(|e| StoreError::Serialization(e.to_string()))
            })
            .collect()
    }

    async fn count_by_correlation_id(&self, correlation_id: &Uuid) -> Result<u64, StoreError> {
        let row: (i64,) = sqlx::query_as(
            r#"
            SELECT COUNT(*) FROM event_log WHERE correlation_id = $1
            "#,
        )
        .bind(correlation_id)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| StoreError::Database(e.to_string()))?;

        Ok(row.0 as u64)
    }
}

// Internal row types for sqlx mapping

#[derive(sqlx::FromRow)]
struct WorkflowRow {
    id: Uuid,
    // ENUMs are fetched as TEXT via `state::text` / `severity::text` casts in
    // the SELECT so sqlx can decode them without needing derive(sqlx::Type) on
    // the Rust enums.
    state: String,
    alert_id: String,
    affected_resource: String,
    severity: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    report_json: Option<serde_json::Value>,
}

impl WorkflowRow {
    fn into_record(self) -> Result<WorkflowRecord, StoreError> {
        Ok(WorkflowRecord {
            id: self.id,
            state: WorkflowState::from_str(&self.state).map_err(StoreError::Serialization)?,
            alert_id: self.alert_id,
            affected_resource: self.affected_resource,
            severity: AlertSeverity::from_str(&self.severity).map_err(StoreError::Serialization)?,
            created_at: self.created_at,
            updated_at: self.updated_at,
            report_json: self.report_json.map(|v| v.to_string()),
        })
    }
}

#[derive(sqlx::FromRow)]
struct EventRow {
    payload_json: serde_json::Value,
}
