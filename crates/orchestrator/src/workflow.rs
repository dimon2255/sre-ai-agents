use chrono::Utc;
use tracing::debug;
use uuid::Uuid;

use sre_common::errors::OrchestratorError;
use sre_common::findings::{Finding, InvestigationReport};
use sre_common::messages::*;
use sre_store::models::{WorkflowRecord, WorkflowState};

/// Manages the lifecycle of a single investigation workflow.
///
/// Each investigation follows the state machine:
/// Created -> Triaging -> Investigating -> Synthesizing -> Resolved/Escalated/Failed
pub struct InvestigationWorkflow {
    pub record: WorkflowRecord,
    /// Accumulated findings from all agents
    findings: Vec<Finding>,
    /// Agents that have been invoked
    agents_invoked: Vec<String>,
    /// Triage result (populated after Triaging state)
    triage_result: Option<TriageResult>,
    /// Number of analysis agents we're waiting for
    pending_agents: u32,
}

impl InvestigationWorkflow {
    /// Create a new investigation workflow from an alert
    pub fn new(alert: &AlertInfo) -> Self {
        let id = Uuid::new_v4();
        let now = Utc::now();

        Self {
            record: WorkflowRecord {
                id,
                state: WorkflowState::Created,
                alert_id: alert.alert_id.clone(),
                affected_resource: alert.affected_resource.clone(),
                severity: alert.severity,
                created_at: now,
                updated_at: now,
                report_json: None,
            },
            findings: Vec::new(),
            agents_invoked: Vec::new(),
            triage_result: None,
            pending_agents: 0,
        }
    }

    pub fn id(&self) -> &Uuid {
        &self.record.id
    }

    pub fn state(&self) -> WorkflowState {
        self.record.state
    }

    pub fn triage_result(&self) -> Option<&TriageResult> {
        self.triage_result.as_ref()
    }

    /// Access accumulated findings (used by engine to decide escalation before report generation)
    pub fn findings(&self) -> &[Finding] {
        &self.findings
    }

    /// Transition to a new state, validating the transition is legal
    pub fn transition_to(&mut self, new_state: WorkflowState) -> Result<(), OrchestratorError> {
        if !self.record.state.can_transition_to(&new_state) {
            return Err(OrchestratorError::InvalidStateTransition {
                from: self.record.state.to_string(),
                to: new_state.to_string(),
            });
        }

        debug!(
            workflow_id = %self.record.id,
            from = %self.record.state,
            to = %new_state,
            "Workflow state transition"
        );

        self.record.state = new_state;
        self.record.updated_at = Utc::now();
        Ok(())
    }

    /// Record a triage result — moves to Investigating if valid
    pub fn record_triage(&mut self, result: TriageResult) {
        self.agents_invoked.push("triage-agent".to_string());
        self.pending_agents = result.recommended_agents.len() as u32;
        self.triage_result = Some(result);
    }

    /// Record an analysis result from an investigation agent
    pub fn record_analysis(&mut self, agent_id: &str, result: &AnalysisResult) {
        self.agents_invoked.push(agent_id.to_string());
        self.findings.extend(result.findings.clone());
        self.pending_agents = self.pending_agents.saturating_sub(1);

        debug!(
            workflow_id = %self.record.id,
            agent_id,
            findings_count = result.findings.len(),
            pending_agents = self.pending_agents,
            "Analysis result recorded"
        );
    }

    /// Check if all expected agents have reported back
    pub fn all_agents_reported(&self) -> bool {
        self.pending_agents == 0
    }

    /// Generate the final investigation report
    pub fn generate_report(&self) -> InvestigationReport {
        let now = Utc::now();
        let duration_ms = (now - self.record.created_at).num_milliseconds().max(0) as u64;

        // Sort findings by severity (Critical first)
        let mut sorted_findings = self.findings.clone();
        sorted_findings.sort_by(|a, b| a.severity.cmp(&b.severity));

        // Calculate overall confidence
        let overall_confidence = if sorted_findings.is_empty() {
            0.0
        } else {
            sorted_findings.iter().map(|f| f.confidence).sum::<f64>() / sorted_findings.len() as f64
        };

        // Build summary
        let summary = self.build_summary(&sorted_findings);
        let escalated = matches!(self.record.state, WorkflowState::Escalated);

        InvestigationReport {
            investigation_id: self.record.id.to_string(),
            started_at: self.record.created_at,
            completed_at: now,
            duration_ms,
            trigger: self.record.alert_id.clone(),
            affected_resource: self.record.affected_resource.clone(),
            overall_confidence,
            findings: sorted_findings,
            summary,
            agents_invoked: self.agents_invoked.clone(),
            escalated,
            escalation_reason: if escalated {
                Some("Low confidence in findings".to_string())
            } else {
                None
            },
        }
    }

    fn build_summary(&self, findings: &[Finding]) -> String {
        if findings.is_empty() {
            return format!(
                "Investigation of {} completed with no significant findings.",
                self.record.affected_resource
            );
        }

        let critical_count = findings
            .iter()
            .filter(|f| matches!(f.severity, sre_common::findings::FindingSeverity::Critical))
            .count();
        let high_count = findings
            .iter()
            .filter(|f| matches!(f.severity, sre_common::findings::FindingSeverity::High))
            .count();

        let mut summary = format!(
            "Investigation of {} found {} findings",
            self.record.affected_resource,
            findings.len()
        );

        if critical_count > 0 || high_count > 0 {
            summary.push_str(&format!(
                " ({critical_count} critical, {high_count} high severity)"
            ));
        }

        summary.push_str(". Top findings: ");

        let top: Vec<&str> = findings.iter().take(3).map(|f| f.title.as_str()).collect();
        summary.push_str(&top.join("; "));

        summary
    }
}
