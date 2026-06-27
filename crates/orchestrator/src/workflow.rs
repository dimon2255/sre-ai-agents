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
        sorted_findings.sort_by_key(|a| a.severity);

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

#[cfg(test)]
mod tests {
    use super::{InvestigationWorkflow, WorkflowState};
    use chrono::Utc;
    use sre_common::errors::OrchestratorError;
    use sre_common::findings::{Finding, FindingCategory, FindingSeverity};
    use sre_common::messages::{
        AgentType, AlertInfo, AlertSeverity, AnalysisResult, InvestigationParams, TriageResult,
    };

    fn alert() -> AlertInfo {
        AlertInfo {
            alert_id: "a-1".to_string(),
            alert_rule: "rule".to_string(),
            severity: AlertSeverity::Sev1,
            signal_type: "Metric".to_string(),
            monitor_condition: "Fired".to_string(),
            affected_resource: "payments-api".to_string(),
            fired_at: Utc::now(),
            description: None,
            raw_context: None,
        }
    }

    fn finding(severity: FindingSeverity, confidence: f64, title: &str) -> Finding {
        Finding {
            category: FindingCategory::ErrorSpike,
            severity,
            title: title.to_string(),
            description: "d".to_string(),
            confidence,
            observed_at: None,
            source_agent: "logs-agent".to_string(),
            evidence: vec![],
            suggestions: vec![],
        }
    }

    fn triage(agents: Vec<AgentType>) -> TriageResult {
        TriageResult {
            severity: AlertSeverity::Sev1,
            affected_service: "payments-api".to_string(),
            signal_summary: "metric".to_string(),
            recommended_agents: agents,
            investigation_params: InvestigationParams {
                resource_name: "payments-api".to_string(),
                resource_group: Some("rg".to_string()),
                timespan_minutes: 30,
                operation_ids: vec![],
            },
        }
    }

    fn analysis(findings: Vec<Finding>) -> AnalysisResult {
        AnalysisResult {
            findings,
            summary: "s".to_string(),
            queries_executed: vec![],
        }
    }

    #[test]
    fn new_starts_in_created_state() {
        let wf = InvestigationWorkflow::new(&alert());
        assert_eq!(wf.state(), WorkflowState::Created);
        assert!(wf.triage_result().is_none());
        assert!(wf.findings().is_empty());
        assert_eq!(*wf.id(), wf.record.id);
        assert!(wf.all_agents_reported()); // 0 pending initially
    }

    #[test]
    fn valid_transition_succeeds() {
        let mut wf = InvestigationWorkflow::new(&alert());
        assert!(wf.transition_to(WorkflowState::Triaging).is_ok());
        assert_eq!(wf.state(), WorkflowState::Triaging);
    }

    #[test]
    fn invalid_transition_is_rejected() {
        let mut wf = InvestigationWorkflow::new(&alert());
        // Created -> Resolved is not a legal transition
        let err = wf.transition_to(WorkflowState::Resolved).unwrap_err();
        assert!(matches!(
            err,
            OrchestratorError::InvalidStateTransition { .. }
        ));
        // state unchanged after a rejected transition
        assert_eq!(wf.state(), WorkflowState::Created);
    }

    #[test]
    fn record_triage_sets_pending_agents() {
        let mut wf = InvestigationWorkflow::new(&alert());
        wf.record_triage(triage(vec![AgentType::Logs, AgentType::Metrics]));
        assert!(wf.triage_result().is_some());
        // two recommended agents => not all reported yet
        assert!(!wf.all_agents_reported());
    }

    #[test]
    fn record_analysis_decrements_pending_and_collects_findings() {
        let mut wf = InvestigationWorkflow::new(&alert());
        wf.record_triage(triage(vec![AgentType::Logs, AgentType::Metrics]));

        wf.record_analysis(
            "logs-agent",
            &analysis(vec![finding(FindingSeverity::High, 0.8, "err spike")]),
        );
        assert!(!wf.all_agents_reported()); // 1 of 2 in
        assert_eq!(wf.findings().len(), 1);

        wf.record_analysis("metrics-agent", &analysis(vec![]));
        assert!(wf.all_agents_reported()); // 2 of 2 in
    }

    #[test]
    fn generate_report_with_no_findings() {
        let wf = InvestigationWorkflow::new(&alert());
        let report = wf.generate_report();
        assert_eq!(report.overall_confidence, 0.0);
        assert!(report.findings.is_empty());
        assert!(report.summary.contains("no significant findings"));
        assert!(!report.escalated); // state is Created, not Escalated
        assert!(report.escalation_reason.is_none());
        assert_eq!(report.affected_resource, "payments-api");
    }

    #[test]
    fn generate_report_sorts_by_severity_and_summarizes() {
        let mut wf = InvestigationWorkflow::new(&alert());
        wf.record_triage(triage(vec![AgentType::Logs]));
        wf.record_analysis(
            "logs-agent",
            &analysis(vec![
                finding(FindingSeverity::Low, 0.5, "low-finding"),
                finding(FindingSeverity::Critical, 0.9, "crit-finding"),
                finding(FindingSeverity::High, 0.8, "high-finding"),
            ]),
        );
        let report = wf.generate_report();
        // Critical sorts first
        assert_eq!(report.findings[0].title, "crit-finding");
        // overall confidence is the arithmetic mean
        assert!((report.overall_confidence - (0.5 + 0.9 + 0.8) / 3.0).abs() < 1e-9);
        assert!(report.summary.contains("3 findings"));
        assert!(report.summary.contains("1 critical"));
        assert!(report.summary.contains("crit-finding"));
        assert!(report.agents_invoked.contains(&"triage-agent".to_string()));
        assert!(report.agents_invoked.contains(&"logs-agent".to_string()));
    }

    #[test]
    fn generate_report_marks_escalated_state() {
        let mut wf = InvestigationWorkflow::new(&alert());
        wf.transition_to(WorkflowState::Triaging).unwrap();
        wf.transition_to(WorkflowState::Escalated).unwrap();
        let report = wf.generate_report();
        assert!(report.escalated);
        assert_eq!(
            report.escalation_reason,
            Some("Low confidence in findings".to_string())
        );
    }
}
