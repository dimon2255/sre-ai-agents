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

    /// Update workflow state
    async fn update_state(
        &self,
        workflow_id: &Uuid,
        state: WorkflowState,
    ) -> Result<(), StoreError>;

    /// Get a workflow by ID
    async fn get_workflow(&self, workflow_id: &Uuid) -> Result<Option<WorkflowRecord>, StoreError>;

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
