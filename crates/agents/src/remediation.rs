use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};
use uuid::Uuid;

use sre_common::errors::AgentError;
use sre_common::findings::{Finding, FindingCategory, FindingSeverity};
use sre_common::messages::*;
use sre_common::remediation::*;

use crate::traits::SreAgent;

/// RemediationAgent — maps investigation findings to remediation *proposals*
/// (Phase 2c, ADR-009). It is **propose-only**: it never executes anything.
/// Each proposal carries a risk classification, an explicit blast-radius
/// (`scope_resources`), an ARM mutation, a rollback strategy, and a rationale.
/// The approval gate (separate crate) decides whether a proposal runs.
pub struct RemediationAgent {
    id: AgentId,
}

impl Default for RemediationAgent {
    fn default() -> Self {
        Self::new()
    }
}

impl RemediationAgent {
    pub fn new() -> Self {
        Self {
            id: AgentId("remediation-agent".to_string()),
        }
    }

    /// Map findings to proposals via the playbook, keeping the highest-confidence
    /// proposal per remediation category (so two error findings don't both
    /// propose a restart).
    fn propose(&self, ctx: &RemediationContext) -> Vec<RemediationProposal> {
        let mut by_category: HashMap<RemediationCategory, RemediationProposal> = HashMap::new();
        for finding in &ctx.findings {
            if let Some(proposal) = self.proposal_for_finding(finding, ctx) {
                // Keep the highest-confidence proposal per category.
                let replace = match by_category.get(&proposal.category) {
                    Some(existing) => proposal.confidence > existing.confidence,
                    None => true,
                };
                if replace {
                    by_category.insert(proposal.category, proposal);
                }
            }
        }
        let mut proposals: Vec<RemediationProposal> = by_category.into_values().collect();
        // Stable order: most confident first.
        proposals.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        proposals
    }

    /// The playbook: which finding shapes have a known safe remediation.
    fn proposal_for_finding(
        &self,
        finding: &Finding,
        ctx: &RemediationContext,
    ) -> Option<RemediationProposal> {
        match finding.category {
            // Error spike (High/Critical) → restart the workload.
            FindingCategory::ErrorSpike if finding.severity <= FindingSeverity::High => {
                Some(self.restart_workload(ctx, finding))
            }
            // Resource exhaustion → scale out by one.
            FindingCategory::ResourceExhaustion => Some(self.scale_compute(ctx, finding)),
            // Latency degradation (High/Critical) → scale out by one.
            FindingCategory::LatencyDegradation if finding.severity <= FindingSeverity::High => {
                Some(self.scale_compute(ctx, finding))
            }
            // A recent deployment correlates with the incident → revert it.
            FindingCategory::DeploymentCorrelation => Some(self.rollback_deployment(ctx, finding)),
            // Everything else (dependency failures, service health, info) has no
            // safe automatic remediation — leave it to a human.
            _ => None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn base(
        &self,
        ctx: &RemediationContext,
        finding: &Finding,
        category: RemediationCategory,
        risk: RiskLevel,
        arm_request: ArmWriteRequest,
        rollback: RollbackStrategy,
        rationale: String,
    ) -> RemediationProposal {
        RemediationProposal {
            id: Uuid::new_v4(),
            investigation_id: ctx.investigation_id,
            proposed_by: self.id.clone(),
            category,
            risk,
            confidence: finding.confidence.clamp(0.0, 1.0),
            scope_resources: vec![ResourceId(ctx.affected_resource.clone())],
            arm_request,
            rollback,
            rationale,
            evidence: vec![finding.title.clone()],
            proposed_at: Utc::now(),
        }
    }

    fn restart_workload(&self, ctx: &RemediationContext, finding: &Finding) -> RemediationProposal {
        self.base(
            ctx,
            finding,
            RemediationCategory::RestartWorkload,
            RiskLevel::LowRisk,
            ArmWriteRequest {
                method: "POST".to_string(),
                resource_id: format!("{}/restart", ctx.affected_resource),
                api_version: "2024-03-01".to_string(),
                body: serde_json::json!({}),
            },
            // A restart is self-reversing — its "prior state" is the running
            // workload, restored by the workload coming back up.
            RollbackStrategy::PriorStateSnapshot {
                captured_at: Utc::now(),
                snapshot: serde_json::json!({
                    "action": "restart",
                    "note": "restart is reversible — the workload returns to service automatically"
                }),
            },
            format!(
                "Error spike on {} ({}); restart the workload to clear transient bad state",
                ctx.params.resource_name, finding.title
            ),
        )
    }

    fn scale_compute(&self, ctx: &RemediationContext, finding: &Finding) -> RemediationProposal {
        // Representative current/target — a production agent would read the
        // current replica count; the proposal records the intended change and
        // the prior value for rollback.
        let current_replicas = 1;
        let target_replicas = current_replicas + 1;
        self.base(
            ctx,
            finding,
            RemediationCategory::ScaleCompute,
            RiskLevel::LowRisk,
            ArmWriteRequest {
                method: "PATCH".to_string(),
                resource_id: ctx.affected_resource.clone(),
                api_version: "2024-03-01".to_string(),
                body: serde_json::json!({
                    "properties": { "template": { "scale": { "minReplicas": target_replicas } } }
                }),
            },
            RollbackStrategy::PriorStateSnapshot {
                captured_at: Utc::now(),
                snapshot: serde_json::json!({ "minReplicas": current_replicas }),
            },
            format!(
                "Resource pressure / latency on {} ({}); scale out {}→{} replicas",
                ctx.params.resource_name, finding.title, current_replicas, target_replicas
            ),
        )
    }

    fn rollback_deployment(
        &self,
        ctx: &RemediationContext,
        finding: &Finding,
    ) -> RemediationProposal {
        self.base(
            ctx,
            finding,
            // Reverting a deployment is bigger blast radius than a restart —
            // single human approval (HighRisk), never auto-approve.
            RemediationCategory::RestartWorkload,
            RiskLevel::HighRisk,
            ArmWriteRequest {
                method: "POST".to_string(),
                resource_id: format!("{}/revisions/previous/activate", ctx.affected_resource),
                api_version: "2024-03-01".to_string(),
                body: serde_json::json!({}),
            },
            RollbackStrategy::PriorStateSnapshot {
                captured_at: Utc::now(),
                snapshot: serde_json::json!({
                    "note": "re-activate the current revision to undo the revert"
                }),
            },
            format!(
                "Recent change correlates with the incident on {} ({}); revert to the previous revision",
                ctx.params.resource_name, finding.title
            ),
        )
    }
}

#[async_trait]
impl SreAgent for RemediationAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Remediation
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "remediation_proposal".to_string(),
            "playbook_mapping".to_string(),
            "rollback_planning".to_string(),
        ]
    }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        info!(agent_id = %self.id, "RemediationAgent started");

        while let Some(inbound) = inbox.recv().await {
            let message = &inbound.message;
            let reply_tx = &inbound.reply_tx;

            debug!(
                message_id = %message.id,
                correlation_id = %message.correlation_id,
                "RemediationAgent received message"
            );

            match &message.payload {
                AgentPayload::RemediationRequest(ctx) => {
                    let proposals = self.propose(ctx);
                    let summary = if proposals.is_empty() {
                        format!(
                            "No safe automatic remediation for {}",
                            ctx.params.resource_name
                        )
                    } else {
                        format!(
                            "{} remediation proposal(s) for {}",
                            proposals.len(),
                            ctx.params.resource_name
                        )
                    };
                    // Confidence of the message = best proposal confidence.
                    let confidence = proposals
                        .iter()
                        .map(|p| p.confidence)
                        .fold(0.0_f64, f64::max);

                    let response = AgentMessage::new(
                        message.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::RemediationProposed(RemediationProposalSet {
                            investigation_id: ctx.investigation_id,
                            proposals,
                            summary,
                        }),
                        confidence,
                    )
                    .with_parent(message.id);

                    if reply_tx.send(response).await.is_err() {
                        warn!("RemediationAgent: reply channel closed");
                        break;
                    }
                }
                AgentPayload::HealthCheck => {
                    let response = AgentMessage::new(
                        message.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::HealthStatus(AgentHealth {
                            agent_id: self.id.clone(),
                            status: AgentStatus::Ready,
                            active_tasks: 0,
                            uptime_secs: 0,
                        }),
                        1.0,
                    );
                    let _ = reply_tx.send(response).await;
                }
                _ => {
                    warn!(agent_id = %self.id, "RemediationAgent received unexpected payload type");
                }
            }
        }

        info!(agent_id = %self.id, "RemediationAgent stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(category: FindingCategory, severity: FindingSeverity, confidence: f64) -> Finding {
        let title = format!("{category:?} finding");
        Finding {
            category,
            severity,
            title,
            description: "desc".to_string(),
            confidence,
            observed_at: None,
            source_agent: "logs-agent".to_string(),
            evidence: vec![],
            suggestions: vec![],
        }
    }

    fn ctx(findings: Vec<Finding>) -> RemediationContext {
        RemediationContext {
            investigation_id: Uuid::nil(),
            affected_resource:
                "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/web"
                    .to_string(),
            findings,
            params: InvestigationParams {
                resource_name: "web".to_string(),
                resource_group: Some("rg".to_string()),
                timespan_minutes: 30,
                operation_ids: vec![],
            },
        }
    }

    #[test]
    fn error_spike_proposes_low_risk_restart() {
        let agent = RemediationAgent::new();
        let proposals = agent.propose(&ctx(vec![finding(
            FindingCategory::ErrorSpike,
            FindingSeverity::Critical,
            0.96,
        )]));
        assert_eq!(proposals.len(), 1);
        let p = &proposals[0];
        assert_eq!(p.category, RemediationCategory::RestartWorkload);
        assert_eq!(p.enforced_risk(), RiskLevel::LowRisk);
        assert_eq!(p.blast_radius(), 1);
        assert!(!p.rollback.is_none());
    }

    #[test]
    fn resource_exhaustion_proposes_scale() {
        let agent = RemediationAgent::new();
        let proposals = agent.propose(&ctx(vec![finding(
            FindingCategory::ResourceExhaustion,
            FindingSeverity::High,
            0.9,
        )]));
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].category, RemediationCategory::ScaleCompute);
        assert_eq!(proposals[0].arm_request.method, "PATCH");
    }

    #[test]
    fn deployment_correlation_is_high_risk() {
        let agent = RemediationAgent::new();
        let proposals = agent.propose(&ctx(vec![finding(
            FindingCategory::DeploymentCorrelation,
            FindingSeverity::Medium,
            0.8,
        )]));
        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].enforced_risk(), RiskLevel::HighRisk);
    }

    #[test]
    fn low_severity_error_spike_and_unmappable_yield_nothing() {
        let agent = RemediationAgent::new();
        // Low-severity error spike is below the restart threshold.
        let low = agent.propose(&ctx(vec![finding(
            FindingCategory::ErrorSpike,
            FindingSeverity::Low,
            0.5,
        )]));
        assert!(low.is_empty());
        // Dependency failure / service health have no auto-remediation.
        let none = agent.propose(&ctx(vec![
            finding(
                FindingCategory::DependencyFailure,
                FindingSeverity::High,
                0.8,
            ),
            finding(
                FindingCategory::ServiceHealth,
                FindingSeverity::Critical,
                0.9,
            ),
        ]));
        assert!(none.is_empty());
    }

    #[test]
    fn duplicate_categories_dedup_to_highest_confidence() {
        let agent = RemediationAgent::new();
        let proposals = agent.propose(&ctx(vec![
            finding(FindingCategory::ErrorSpike, FindingSeverity::High, 0.7),
            finding(FindingCategory::ErrorSpike, FindingSeverity::Critical, 0.95),
        ]));
        assert_eq!(proposals.len(), 1);
        assert!((proposals[0].confidence - 0.95).abs() < f64::EPSILON);
    }
}
