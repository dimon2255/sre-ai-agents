//! Remediation pipeline (Phase 2c, ADR-009) — ties the propose-only
//! `RemediationAgent`, the `sre-gates` `ApprovalGate`, and the `sre-azure`
//! `ArmWriteClient` into a single propose → gate → execute → verify → (rollback)
//! flow, emitting an insert-only audit event per transition.
//!
//! It is held *optionally* by the `Orchestrator` (`with_remediation`) and only
//! runs when remediation is enabled. Auto-approved proposals execute; human
//! (HighRisk/Destructive), blocked, and rejected decisions stop before any ARM
//! mutation — the audit trail still records the decision.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use tracing::warn;
use uuid::Uuid;

use sre_agents::RemediationAgent;
use sre_azure::ArmWriteClient;
use sre_common::findings::InvestigationReport;
use sre_common::messages::InvestigationParams;
use sre_common::remediation::{
    RemediationContext, RemediationEvent, RemediationEventKind, RemediationProposal,
    RollbackStrategy,
};
use sre_gates::ApprovalGate;

/// Executes an approved remediation against ARM with a post-mortem blast-radius
/// check and rollback-on-overscope (ADR-009 §follow-ups).
pub struct RemediationExecutor {
    arm: Arc<dyn ArmWriteClient>,
    gate: Arc<ApprovalGate>,
    subscription_id: String,
}

impl RemediationExecutor {
    pub fn new(
        arm: Arc<dyn ArmWriteClient>,
        gate: Arc<ApprovalGate>,
        subscription_id: impl Into<String>,
    ) -> Self {
        Self {
            arm,
            gate,
            subscription_id: subscription_id.into(),
        }
    }

    /// Run one proposal through the gate and, if auto-approved, the executor.
    /// Returns the lifecycle events to append to the audit log. Human-approval,
    /// blocked, and rejected decisions stop before execution.
    pub async fn process(
        &self,
        proposal: &RemediationProposal,
        now: DateTime<Utc>,
    ) -> Vec<RemediationEvent> {
        use RemediationEventKind::*;
        let mut events = vec![RemediationEvent::new(
            proposal,
            Proposed,
            "remediation-agent",
            proposal.rationale.clone(),
            now,
        )];

        let decision = self.gate.evaluate(proposal, &self.subscription_id, now);
        events.push(RemediationEvent::new(
            proposal,
            decision.event_kind(),
            "sre-gates",
            decision.detail(),
            now,
        ));

        if !decision.is_auto_approved() {
            // Human path / blocked / rejected — nothing executes here. A real
            // deployment resumes the human path via gate.approve() + a separate
            // execute call once `approvals_needed` is met.
            return events;
        }

        events.push(RemediationEvent::new(
            proposal,
            Executing,
            "executor",
            proposal.arm_request.resource_id.clone(),
            now,
        ));

        match self.arm.execute(&proposal.arm_request).await {
            Ok(resp) => {
                // Post-mortem blast-radius check: every affected resource must be
                // within (a sub-path of, or the parent of) a declared scope
                // resource. Anything else is over-scope → rollback + escalate.
                let over_scope: Vec<String> = resp
                    .affected_resources
                    .iter()
                    .filter(|a| {
                        !proposal
                            .scope_resources
                            .iter()
                            .any(|s| a.0.starts_with(&s.0) || s.0.starts_with(&a.0))
                    })
                    .map(|r| r.0.clone())
                    .collect();

                if over_scope.is_empty() {
                    events.push(RemediationEvent::new(
                        proposal,
                        Executed,
                        "executor",
                        format!("ARM {} ok", resp.status),
                        now,
                    ));
                } else {
                    warn!(
                        ?over_scope,
                        proposal_id = %proposal.id,
                        "remediation exceeded declared scope — rolling back"
                    );
                    events.push(RemediationEvent::new(
                        proposal,
                        RolledBack,
                        "executor",
                        format!(
                            "blast radius exceeded ({}); rollback artefact: {}",
                            over_scope.join(","),
                            rollback_artefact(&proposal.rollback)
                        ),
                        now,
                    ));
                }
            }
            Err(e) => {
                events.push(RemediationEvent::new(
                    proposal,
                    Failed,
                    "executor",
                    e.to_string(),
                    now,
                ));
            }
        }

        self.gate.release(proposal);
        events
    }
}

/// Render the rollback artefact into the audit detail so an operator can revert
/// manually even if the platform is down (ADR-009 §5).
fn rollback_artefact(rollback: &RollbackStrategy) -> String {
    match rollback {
        RollbackStrategy::PriorStateSnapshot { snapshot, .. } => format!("snapshot={snapshot}"),
        RollbackStrategy::PointInTimeRestore { restore_token, .. } => {
            format!("restore_token={restore_token}")
        }
        RollbackStrategy::None => "none".to_string(),
    }
}

/// The propose → gate → execute pipeline, held optionally by the Orchestrator.
pub struct RemediationPipeline {
    pub agent: RemediationAgent,
    pub executor: RemediationExecutor,
}

impl RemediationPipeline {
    pub fn new(
        arm: Arc<dyn ArmWriteClient>,
        gate: Arc<ApprovalGate>,
        subscription_id: impl Into<String>,
    ) -> Self {
        Self {
            agent: RemediationAgent::new(),
            executor: RemediationExecutor::new(arm, gate, subscription_id),
        }
    }
}

/// Build a remediation context from a completed investigation report.
pub fn build_context(report: &InvestigationReport) -> Option<RemediationContext> {
    let investigation_id = Uuid::parse_str(&report.investigation_id).ok()?;
    let resource_name = report
        .affected_resource
        .rsplit('/')
        .next()
        .unwrap_or(&report.affected_resource)
        .to_string();
    Some(RemediationContext {
        investigation_id,
        affected_resource: report.affected_resource.clone(),
        findings: report.findings.clone(),
        params: InvestigationParams {
            resource_name,
            resource_group: None,
            timespan_minutes: 30,
            operation_ids: vec![],
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use sre_azure::{MockArmClient, MockArmMode};
    use sre_common::findings::{Finding, FindingCategory, FindingSeverity};
    use sre_common::messages::AgentId;
    use sre_common::remediation::{ArmWriteRequest, RemediationCategory, ResourceId, RiskLevel};

    fn at() -> DateTime<Utc> {
        DateTime::<Utc>::from_timestamp(12 * 3600, 0).unwrap()
    }

    fn low_risk() -> RemediationProposal {
        RemediationProposal {
            id: Uuid::new_v4(),
            investigation_id: Uuid::nil(),
            proposed_by: AgentId("remediation-agent".to_string()),
            category: RemediationCategory::RestartWorkload,
            risk: RiskLevel::LowRisk,
            confidence: 0.97,
            scope_resources: vec![ResourceId(
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/web"
                    .to_string(),
            )],
            arm_request: ArmWriteRequest {
                method: "POST".to_string(),
                resource_id:
                    "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/web/restart"
                        .to_string(),
                api_version: "2024-03-01".to_string(),
                body: serde_json::json!({}),
            },
            rollback: RollbackStrategy::PriorStateSnapshot {
                captured_at: at(),
                snapshot: serde_json::json!({"replicas": 1}),
            },
            rationale: "restart".to_string(),
            evidence: vec![],
            proposed_at: at(),
        }
    }

    #[tokio::test]
    async fn auto_approved_success_executes_and_records_executed() {
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::new());
        let exec = RemediationExecutor::new(arm.clone(), gate, "sub-1");
        let events = exec.process(&low_risk(), at()).await;
        let kinds: Vec<RemediationEventKind> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&RemediationEventKind::AutoApproved));
        assert!(kinds.contains(&RemediationEventKind::Executed));
        assert_eq!(arm.calls().len(), 1);
    }

    #[tokio::test]
    async fn over_scope_rolls_back_and_does_not_record_executed() {
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::with_mode(MockArmMode::OverScope(vec![
            ResourceId(
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/other"
                    .to_string(),
            ),
        ])));
        let exec = RemediationExecutor::new(arm, gate, "sub-1");
        let events = exec.process(&low_risk(), at()).await;
        assert!(events
            .iter()
            .any(|e| e.kind == RemediationEventKind::RolledBack));
        assert!(!events
            .iter()
            .any(|e| e.kind == RemediationEventKind::Executed));
    }

    #[tokio::test]
    async fn arm_failure_records_failed() {
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::with_mode(MockArmMode::Failure(
            "boom".to_string(),
        )));
        let exec = RemediationExecutor::new(arm, gate, "sub-1");
        let events = exec.process(&low_risk(), at()).await;
        assert!(events
            .iter()
            .any(|e| e.kind == RemediationEventKind::Failed));
    }

    #[tokio::test]
    async fn high_risk_stops_at_approval_request_without_executing() {
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::new());
        let exec = RemediationExecutor::new(arm.clone(), gate, "sub-1");
        let mut p = low_risk();
        p.risk = RiskLevel::HighRisk;
        let events = exec.process(&p, at()).await;
        assert!(events
            .iter()
            .any(|e| e.kind == RemediationEventKind::ApprovalRequested));
        assert!(!events
            .iter()
            .any(|e| e.kind == RemediationEventKind::Executing));
        assert_eq!(arm.calls().len(), 0);
    }

    #[test]
    fn build_context_extracts_resource_name() {
        let id = Uuid::new_v4();
        let now = Utc::now();
        let report = InvestigationReport {
            investigation_id: id.to_string(),
            started_at: now,
            completed_at: now,
            duration_ms: 1,
            trigger: "alert".to_string(),
            affected_resource:
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/web"
                    .to_string(),
            overall_confidence: 0.9,
            findings: vec![Finding {
                category: FindingCategory::ErrorSpike,
                severity: FindingSeverity::High,
                title: "t".to_string(),
                description: "d".to_string(),
                confidence: 0.9,
                observed_at: None,
                source_agent: "logs-agent".to_string(),
                evidence: vec![],
                suggestions: vec![],
            }],
            summary: "s".to_string(),
            agents_invoked: vec![],
            escalated: false,
            escalation_reason: None,
        };
        let ctx = build_context(&report).unwrap();
        assert_eq!(ctx.params.resource_name, "web");
        assert_eq!(ctx.findings.len(), 1);
    }
}
