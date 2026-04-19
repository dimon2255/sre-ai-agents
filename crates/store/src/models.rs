use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use sre_common::messages::AlertSeverity;

/// Persisted workflow record
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRecord {
    /// Workflow / investigation ID (same as correlation_id)
    pub id: Uuid,
    /// Current state
    pub state: WorkflowState,
    /// Alert that triggered this investigation
    pub alert_id: String,
    /// Affected resource ARM ID or name
    pub affected_resource: String,
    /// Alert severity
    pub severity: AlertSeverity,
    /// When the workflow was created
    pub created_at: DateTime<Utc>,
    /// When the workflow last changed state
    pub updated_at: DateTime<Utc>,
    /// Final report JSON (populated when resolved)
    pub report_json: Option<String>,
}

/// Workflow state machine states (ADR-001)
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowState {
    /// Investigation created, not yet started
    Created,
    /// TriageAgent is classifying the alert
    Triaging,
    /// Investigation agents are analyzing in parallel
    Investigating,
    /// Findings are being aggregated
    Synthesizing,
    /// Investigation complete with findings
    Resolved,
    /// Escalated to humans due to low confidence
    Escalated,
    /// Investigation failed due to errors
    Failed,
}

impl WorkflowState {
    /// Check if this is a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Resolved | Self::Escalated | Self::Failed)
    }

    /// Valid transitions from this state
    pub fn valid_transitions(&self) -> &[WorkflowState] {
        match self {
            Self::Created => &[Self::Triaging, Self::Failed],
            Self::Triaging => &[Self::Investigating, Self::Escalated, Self::Failed],
            Self::Investigating => &[Self::Synthesizing, Self::Failed],
            Self::Synthesizing => &[Self::Resolved, Self::Escalated, Self::Failed],
            Self::Resolved | Self::Escalated | Self::Failed => &[],
        }
    }

    /// Check if transitioning to `next` is valid
    pub fn can_transition_to(&self, next: &WorkflowState) -> bool {
        self.valid_transitions().contains(next)
    }
}

impl std::fmt::Display for WorkflowState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Created => write!(f, "created"),
            Self::Triaging => write!(f, "triaging"),
            Self::Investigating => write!(f, "investigating"),
            Self::Synthesizing => write!(f, "synthesizing"),
            Self::Resolved => write!(f, "resolved"),
            Self::Escalated => write!(f, "escalated"),
            Self::Failed => write!(f, "failed"),
        }
    }
}

impl std::str::FromStr for WorkflowState {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "created" => Ok(Self::Created),
            "triaging" => Ok(Self::Triaging),
            "investigating" => Ok(Self::Investigating),
            "synthesizing" => Ok(Self::Synthesizing),
            "resolved" => Ok(Self::Resolved),
            "escalated" => Ok(Self::Escalated),
            "failed" => Ok(Self::Failed),
            other => Err(format!("unknown WorkflowState label: {other}")),
        }
    }
}
