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
use sre_gates::{ApprovalGate, TrustModel};

/// Unreachable confidence floor used to suspend LowRisk auto-approval while a
/// category is in a progressive-trust cooldown — forces the gate's normal
/// human-approval routing without special-casing the decision (ADR-017).
const COOLDOWN_FLOOR: f64 = 1.01;

/// Executes an approved remediation against ARM with a post-mortem blast-radius
/// check and rollback-on-overscope (ADR-009 §follow-ups).
///
/// When a [`TrustModel`] is attached (Phase 3 auto-remediation, ADR-017) the
/// executor consults it for the per-category auto-approve confidence floor and
/// feeds each execution's outcome back so trusted patterns earn lower floors over
/// time. Without one it behaves exactly as in Phase 2c (the gate's configured
/// floor, no learning).
pub struct RemediationExecutor {
    arm: Arc<dyn ArmWriteClient>,
    gate: Arc<ApprovalGate>,
    subscription_id: String,
    trust: Option<Arc<TrustModel>>,
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
            trust: None,
        }
    }

    /// Attach a progressive-trust model (Phase 3, ADR-017). Builder style.
    pub fn with_trust(mut self, trust: Arc<TrustModel>) -> Self {
        self.trust = Some(trust);
        self
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

        // Progressive trust (ADR-017): a trusted category gets a lower auto-approve
        // floor; one in cooldown gets an unreachable floor that routes it to human
        // approval. Without a trust model, fall back to the gate's configured floor.
        let decision = match self.trust.as_ref() {
            Some(trust) => {
                let floor = trust
                    .threshold_for(proposal.category, now)
                    .unwrap_or(COOLDOWN_FLOOR);
                self.gate.evaluate_with_confidence_floor(
                    proposal,
                    &self.subscription_id,
                    now,
                    floor,
                )
            }
            None => self.gate.evaluate(proposal, &self.subscription_id, now),
        };
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

        let mut exec_success = false;
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
                    exec_success = true;
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

        // Progressive trust (ADR-017): feed the outcome back so the category's
        // auto-approve floor adapts — success climbs the trust tiers; an
        // over-scope rollback or ARM failure resets and triggers a cooldown.
        if let Some(trust) = self.trust.as_ref() {
            trust.record(proposal.category, exec_success, now);
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

    /// Enable progressive-trust auto-remediation (Phase 3, ADR-017). Builder
    /// style: the executor consults the trust model for the per-category
    /// auto-approve floor and feeds outcomes back to it.
    pub fn with_trust(mut self, trust: Arc<TrustModel>) -> Self {
        self.executor = self.executor.with_trust(trust);
        self
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

    // ── Progressive trust (ADR-017) ──

    fn with_conf(mut p: RemediationProposal, c: f64) -> RemediationProposal {
        p.confidence = c;
        p
    }

    const RW: RemediationCategory = RemediationCategory::RestartWorkload;

    #[tokio::test]
    async fn untrusted_category_routes_sub_base_confidence_to_approval() {
        // No trust model: the fixed 0.95 floor applies, so 0.94 needs a human.
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::new());
        let exec = RemediationExecutor::new(arm.clone(), gate, "sub-1");
        let events = exec.process(&with_conf(low_risk(), 0.94), at()).await;
        assert!(events
            .iter()
            .any(|e| e.kind == RemediationEventKind::ApprovalRequested));
        assert_eq!(arm.calls().len(), 0);
    }

    #[tokio::test]
    async fn trusted_category_auto_approves_below_base_floor() {
        let trust = Arc::new(TrustModel::with_defaults());
        for _ in 0..10 {
            trust.record(RW, true, at()); // earns the 0.93 floor
        }
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::new());
        let exec = RemediationExecutor::new(arm.clone(), gate, "sub-1").with_trust(trust.clone());
        // 0.94 is below the 0.95 base but above the earned 0.93 floor.
        let events = exec.process(&with_conf(low_risk(), 0.94), at()).await;
        let kinds: Vec<RemediationEventKind> = events.iter().map(|e| e.kind).collect();
        assert!(kinds.contains(&RemediationEventKind::AutoApproved));
        assert!(kinds.contains(&RemediationEventKind::Executed));
        assert_eq!(arm.calls().len(), 1);
        // Outcome fed back: 10 prior + this success = 11.
        assert_eq!(trust.successes(RW), 11);
    }

    #[tokio::test]
    async fn category_in_cooldown_routes_to_approval_even_at_high_confidence() {
        let trust = Arc::new(TrustModel::with_defaults());
        for _ in 0..10 {
            trust.record(RW, true, at());
        }
        trust.record(RW, false, at()); // failure → cooldown, auto-remediation suspended
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::new());
        let exec = RemediationExecutor::new(arm.clone(), gate, "sub-1").with_trust(trust);
        // Even 0.99 confidence can't auto-approve during cooldown.
        let events = exec.process(&with_conf(low_risk(), 0.99), at()).await;
        assert!(events
            .iter()
            .any(|e| e.kind == RemediationEventKind::ApprovalRequested));
        assert_eq!(arm.calls().len(), 0);
    }

    #[tokio::test]
    async fn arm_failure_feeds_back_and_resets_trust() {
        let trust = Arc::new(TrustModel::with_defaults());
        for _ in 0..5 {
            trust.record(RW, true, at()); // still base floor (0.95), successes = 5
        }
        let gate = Arc::new(ApprovalGate::with_defaults());
        let arm = Arc::new(MockArmClient::with_mode(MockArmMode::Failure(
            "boom".to_string(),
        )));
        let exec = RemediationExecutor::new(arm, gate, "sub-1").with_trust(trust.clone());
        // 0.97 ≥ 0.95 base → auto-approves, then ARM fails.
        let _ = exec.process(&low_risk(), at()).await;
        assert_eq!(
            trust.successes(RW),
            0,
            "an ARM failure must reset consecutive successes"
        );
    }
}
