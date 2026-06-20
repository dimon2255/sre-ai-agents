use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A single finding from an agent's analysis
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Finding {
    /// What category of finding this is
    pub category: FindingCategory,
    /// Severity of this finding
    pub severity: FindingSeverity,
    /// Human-readable title
    pub title: String,
    /// Detailed description of what was found
    pub description: String,
    /// Confidence score (0.0-1.0)
    pub confidence: f64,
    /// When the issue was observed (from telemetry)
    pub observed_at: Option<DateTime<Utc>>,
    /// Which agent produced this finding
    pub source_agent: String,
    /// Supporting evidence (metric values, log excerpts, etc.)
    pub evidence: Vec<Evidence>,
    /// Suggested next steps (for human consumption in Phase 1)
    pub suggestions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingCategory {
    /// Error rate or exception spike
    ErrorSpike,
    /// Latency degradation
    LatencyDegradation,
    /// Resource exhaustion (CPU, memory, disk, connections)
    ResourceExhaustion,
    /// Dependency failure (downstream service, database, etc.)
    DependencyFailure,
    /// Recent deployment correlated with incident
    DeploymentCorrelation,
    /// Traffic anomaly (spike or drop)
    TrafficAnomaly,
    /// Configuration change detected
    ConfigChange,
    /// Resource/service health degradation (Azure Resource Health transition)
    ServiceHealth,
    /// General informational finding
    Informational,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum FindingSeverity {
    Critical,
    High,
    Medium,
    Low,
    Info,
}

impl std::fmt::Display for FindingSeverity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Critical => write!(f, "CRITICAL"),
            Self::High => write!(f, "HIGH"),
            Self::Medium => write!(f, "MEDIUM"),
            Self::Low => write!(f, "LOW"),
            Self::Info => write!(f, "INFO"),
        }
    }
}

impl Finding {
    /// Tag this finding with the data scope it was produced from — a Log
    /// Analytics workspace id or an App Insights application id (Phase 3
    /// multi-workspace investigation, ADR-020). Attaches the scope as evidence so
    /// a finding surfaced from one of several workspaces is attributable.
    pub fn with_source_scope(mut self, scope: &str) -> Self {
        self.evidence.push(Evidence {
            evidence_type: EvidenceType::QueryResult,
            label: "source_scope".to_string(),
            data: serde_json::json!({ "scope": scope }),
        });
        self
    }

    /// The source scopes (workspace / app ids) tagged on this finding, if any.
    pub fn source_scopes(&self) -> Vec<String> {
        self.evidence
            .iter()
            .filter(|e| e.label == "source_scope")
            .filter_map(|e| {
                e.data
                    .get("scope")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect()
    }
}

/// Supporting evidence attached to a finding
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evidence {
    /// What kind of evidence this is
    pub evidence_type: EvidenceType,
    /// Label for display
    pub label: String,
    /// The evidence data
    pub data: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceType {
    /// A metric value or timeseries
    Metric,
    /// Log line(s)
    LogExcerpt,
    /// A KQL query result table
    QueryResult,
    /// A timestamp correlation
    TimeCorrelation,
}

/// Aggregated investigation report — the final output of an investigation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvestigationReport {
    /// Investigation correlation ID
    pub investigation_id: String,
    /// When the investigation started
    pub started_at: DateTime<Utc>,
    /// When the investigation completed
    pub completed_at: DateTime<Utc>,
    /// Duration in milliseconds
    pub duration_ms: u64,
    /// The alert that triggered this investigation
    pub trigger: String,
    /// Affected resource/service
    pub affected_resource: String,
    /// Overall confidence in the findings
    pub overall_confidence: f64,
    /// All findings, sorted by severity
    pub findings: Vec<Finding>,
    /// Summary for human consumption
    pub summary: String,
    /// Which agents participated
    pub agents_invoked: Vec<String>,
    /// Whether the investigation escalated
    pub escalated: bool,
    /// Escalation reason if applicable
    pub escalation_reason: Option<String>,
}
