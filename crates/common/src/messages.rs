use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::findings::Finding;

/// Unique identifier for an agent instance
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentId(pub String);

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// Agent type classification
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentType {
    Triage,
    Logs,
    Metrics,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Triage => write!(f, "triage"),
            Self::Logs => write!(f, "logs"),
            Self::Metrics => write!(f, "metrics"),
        }
    }
}

/// Where a message should be delivered
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageTarget {
    Agent(AgentId),
    Orchestrator,
    Broadcast,
}

/// Structured message envelope for all agent communication (ADR-003, ADR-006)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentMessage {
    /// Unique message ID
    pub id: Uuid,
    /// Links all messages in one investigation
    pub correlation_id: Uuid,
    /// Which agent sent this message
    pub source_agent: AgentId,
    /// Intended recipient
    pub target: MessageTarget,
    /// Typed payload
    pub payload: AgentPayload,
    /// Agent's confidence in this message (0.0-1.0)
    pub confidence: f64,
    /// When the message was created
    pub timestamp: DateTime<Utc>,
    /// Parent message (forms a DAG of reasoning)
    pub parent_message_id: Option<Uuid>,
}

/// Inbound task message bundled with a per-investigation reply channel (ADR-006).
///
/// The reply_tx is scoped to the investigation that dispatched this task,
/// eliminating the shared-inbox race condition from the original design.
pub struct InboundMessage {
    pub message: AgentMessage,
    pub reply_tx: tokio::sync::mpsc::Sender<AgentMessage>,
}

impl AgentMessage {
    pub fn new(
        correlation_id: Uuid,
        source_agent: AgentId,
        target: MessageTarget,
        payload: AgentPayload,
        confidence: f64,
    ) -> Self {
        Self {
            id: Uuid::new_v4(),
            correlation_id,
            source_agent,
            target,
            payload,
            confidence,
            timestamp: Utc::now(),
            parent_message_id: None,
        }
    }

    pub fn with_parent(mut self, parent_id: Uuid) -> Self {
        self.parent_message_id = Some(parent_id);
        self
    }
}

/// All possible message payloads between agents and orchestrator
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AgentPayload {
    /// Orchestrator -> TriageAgent: new investigation request
    InvestigationRequest(InvestigationContext),
    /// TriageAgent -> Orchestrator: triage results with investigation plan
    TriageResult(TriageResult),
    /// Orchestrator -> Agent: analyze specific aspect
    AnalysisTask(AnalysisTask),
    /// Agent -> Orchestrator: findings from analysis
    AnalysisResult(AnalysisResult),
    /// Agent -> Orchestrator: agent needs to escalate
    EscalationNotice(EscalationNotice),
    /// Orchestrator -> Agent: health check ping
    HealthCheck,
    /// Agent -> Orchestrator: health check response
    HealthStatus(AgentHealth),
}

/// Context for a new investigation, typically from an Azure alert
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvestigationContext {
    /// The raw alert that triggered this investigation
    pub alert: AlertInfo,
    /// Optional: additional context from the trigger
    pub additional_context: Option<String>,
}

/// Normalized alert information from Azure Common Alert Schema
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AlertInfo {
    pub alert_id: String,
    pub alert_rule: String,
    pub severity: AlertSeverity,
    pub signal_type: String,
    pub monitor_condition: String,
    pub affected_resource: String,
    pub fired_at: DateTime<Utc>,
    pub description: Option<String>,
    /// Raw alert context payload for agent-specific parsing
    pub raw_context: Option<serde_json::Value>,
}

/// Azure alert severity levels
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub enum AlertSeverity {
    Sev0,
    Sev1,
    Sev2,
    Sev3,
    Sev4,
}

impl AlertSeverity {
    /// Canonical short label: matches the `alert_severity` Postgres ENUM
    /// values and the Azure Common Alert Schema `severity` field. Use this
    /// when persisting or comparing; `Display` is reserved for the verbose
    /// human-readable form.
    pub fn as_label(&self) -> &'static str {
        match self {
            Self::Sev0 => "Sev0",
            Self::Sev1 => "Sev1",
            Self::Sev2 => "Sev2",
            Self::Sev3 => "Sev3",
            Self::Sev4 => "Sev4",
        }
    }
}

impl std::fmt::Display for AlertSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sev0 => write!(f, "Sev0-Critical"),
            Self::Sev1 => write!(f, "Sev1-Error"),
            Self::Sev2 => write!(f, "Sev2-Warning"),
            Self::Sev3 => write!(f, "Sev3-Informational"),
            Self::Sev4 => write!(f, "Sev4-Verbose"),
        }
    }
}

impl std::str::FromStr for AlertSeverity {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Sev0" => Ok(Self::Sev0),
            "Sev1" => Ok(Self::Sev1),
            "Sev2" => Ok(Self::Sev2),
            "Sev3" => Ok(Self::Sev3),
            "Sev4" => Ok(Self::Sev4),
            other => Err(format!("unknown AlertSeverity label: {other}")),
        }
    }
}

/// Result of triage classification
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageResult {
    /// Classified severity (may differ from alert severity)
    pub severity: AlertSeverity,
    /// Identified affected service/resource
    pub affected_service: String,
    /// Signal type that triggered the investigation
    pub signal_summary: String,
    /// Which agents should be invoked for investigation
    pub recommended_agents: Vec<AgentType>,
    /// Extracted parameters for downstream agents
    pub investigation_params: InvestigationParams,
}

/// Parameters extracted by triage for downstream agents
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvestigationParams {
    /// Resource or service name to investigate
    pub resource_name: String,
    /// Resource group (if applicable)
    pub resource_group: Option<String>,
    /// Timespan to look back (minutes)
    pub timespan_minutes: u32,
    /// Specific operation IDs to trace (if available from alert context)
    pub operation_ids: Vec<String>,
}

/// Task assigned to an investigation agent
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisTask {
    /// What kind of analysis to perform
    pub task_type: AnalysisTaskType,
    /// Parameters from triage
    pub params: InvestigationParams,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisTaskType {
    /// Analyze error logs and patterns
    ErrorAnalysis,
    /// Analyze performance metrics (latency, throughput, failure rate)
    PerformanceAnalysis,
    /// Analyze resource utilization metrics
    ResourceUtilization,
}

/// Result from an investigation agent's analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AnalysisResult {
    /// Findings from the analysis
    pub findings: Vec<Finding>,
    /// Summary of what was analyzed
    pub summary: String,
    /// Queries that were executed (for audit trail)
    pub queries_executed: Vec<QueryRecord>,
}

/// Record of a query executed during analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryRecord {
    pub template_name: String,
    pub parameters: serde_json::Value,
    pub rows_returned: u32,
    pub duration_ms: u64,
}

/// Escalation when confidence is too low
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EscalationNotice {
    pub reason: EscalationReason,
    pub details: String,
    pub partial_findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EscalationReason {
    LowConfidence,
    InsufficientData,
    UnknownPattern,
    ToolFailure,
    Timeout,
}

/// Agent health status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentHealth {
    pub agent_id: AgentId,
    pub status: AgentStatus,
    pub active_tasks: u32,
    pub uptime_secs: u64,
}

/// Agent operational status
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Ready,
    Busy,
    Degraded,
    Failed,
}
