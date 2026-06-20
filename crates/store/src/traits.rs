use async_trait::async_trait;
use sre_common::errors::StoreError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::AgentMessage;
use uuid::Uuid;

use crate::models::{WorkflowRecord, WorkflowState};

/// Persistence interface for investigation workflows
#[async_trait]
pub trait WorkflowStore: Send + Sync {
    /// Create a new workflow record
    async fn create_workflow(&self, workflow: &WorkflowRecord) -> Result<(), StoreError>;

    /// Attempt to create a workflow record, returning `Ok(true)` on insert
    /// and `Ok(false)` when an existing workflow with the same `alert_id`
    /// already exists. This is the cross-replica dedup gate (ADR-013) —
    /// the in-memory LRU is a fast path in front of this; the DB unique
    /// constraint is the authoritative record.
    async fn try_create_workflow(&self, workflow: &WorkflowRecord) -> Result<bool, StoreError>;

    /// Update workflow state
    async fn update_state(
        &self,
        workflow_id: &Uuid,
        state: WorkflowState,
    ) -> Result<(), StoreError>;

    /// Get a workflow by ID
    async fn get_workflow(&self, workflow_id: &Uuid) -> Result<Option<WorkflowRecord>, StoreError>;

    /// Fetch the most recent workflow for an alert id (ChatOps status lookup by
    /// the tracking reference returned when an investigation is triggered).
    async fn get_workflow_by_alert_id(
        &self,
        alert_id: &str,
    ) -> Result<Option<WorkflowRecord>, StoreError>;

    /// Get active (non-terminal) workflows
    async fn get_active_workflows(&self) -> Result<Vec<WorkflowRecord>, StoreError>;

    /// Store the final investigation report
    async fn store_report(
        &self,
        workflow_id: &Uuid,
        report: &InvestigationReport,
    ) -> Result<(), StoreError>;
}

/// Append-only event log for audit trail
#[async_trait]
pub trait EventLog: Send + Sync {
    /// Append a message to the event log
    async fn append(&self, message: &AgentMessage) -> Result<(), StoreError>;

    /// Get all events for a correlation ID (full investigation trace)
    async fn get_by_correlation_id(
        &self,
        correlation_id: &Uuid,
    ) -> Result<Vec<AgentMessage>, StoreError>;

    /// Get event count for a correlation ID
    async fn count_by_correlation_id(&self, correlation_id: &Uuid) -> Result<u64, StoreError>;
}
