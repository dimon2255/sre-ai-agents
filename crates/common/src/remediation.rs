//! Remediation types (Phase 2c, ADR-009).
//!
//! These types travel inside `AgentPayload` so every crate that constructs or
//! routes a remediation can carry the risk classification end-to-end without a
//! dependency cycle (the enforcement lives in `sre-gates`; the data lives
//! here). Nothing in this module executes anything — it is the vocabulary of
//! the gated-remediation pipeline.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::findings::Finding;
use crate::messages::{AgentId, InvestigationParams};

/// Classification of a proposed remediation action (ADR-009 §1). Ordered from
/// least to most dangerous; the approval gate uses this to decide auto-approve
/// vs. human routing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// No side effects on Azure state. Auto-approved unconditionally.
    ReadOnly,
    /// Reversible, bounded, non-data-touching (restart, scale ±1). Auto-approve
    /// iff confidence >= 0.95 AND scope <= 1 AND rate-limit OK.
    LowRisk,
    /// Larger blast radius config/capacity change. Always single human approval.
    HighRisk,
    /// Irreversible or identity/data/observability touching. Always dual
    /// approval; never auto-approves. No-rollback actions fall here.
    Destructive,
}

/// What kind of remediation an action is (ADR-009 §2). Some categories are
/// *always* `Destructive`; one is blocklisted entirely.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum RemediationCategory {
    RestartWorkload,
    ScaleCompute,
    RotateCertificate,
    PurgeCache,
    ClearQueue,
    /// Always Destructive.
    DeleteResource,
    /// Always Destructive — platform may never mutate identity resources.
    IdentityMutation,
    /// Always Destructive — platform may never remove access.
    AccessRemoval,
    /// BLOCKLISTED — the platform may never mutate its own observability;
    /// proposals of this shape are rejected before they reach the gate.
    ObservabilityMutation,
}

impl RemediationCategory {
    /// Categories that are `Destructive` regardless of rollback/scope (ADR-009
    /// threat model T1485/T1098/T1531).
    pub fn is_always_destructive(self) -> bool {
        matches!(
            self,
            Self::DeleteResource | Self::IdentityMutation | Self::AccessRemoval
        )
    }

    /// Categories rejected before the gate (ADR-009 T1562 — Impair Defenses).
    pub fn is_blocklisted(self) -> bool {
        matches!(self, Self::ObservabilityMutation)
    }
}

/// An ARM resource identifier (ARM resource id or path).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ResourceId(pub String);

impl std::fmt::Display for ResourceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The actual mutation an executor would issue against ARM. Carried in the
/// proposal so the gate and audit trail see exactly what would run; the
/// executor (Phase 2c, separate PR) is the only thing that issues it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ArmWriteRequest {
    /// HTTP method: `PATCH` | `POST` | `DELETE`.
    pub method: String,
    /// Target ARM resource id / action path.
    pub resource_id: String,
    /// ARM API version.
    pub api_version: String,
    /// Request body (empty object for parameterless actions like restart).
    pub body: serde_json::Value,
}

/// How a remediation can be undone (ADR-009 §2/§5). Not optional — a proposal
/// that cannot determine one emits `None`, which forces `RiskLevel::Destructive`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RollbackStrategy {
    /// Capture prior state, execute, retain prior state for reversal.
    PriorStateSnapshot {
        captured_at: DateTime<Utc>,
        snapshot: serde_json::Value,
    },
    /// Azure-native point-in-time restore; carries the restore identifier.
    PointInTimeRestore {
        resource: ResourceId,
        restore_token: String,
    },
    /// No rollback possible. Forces `RiskLevel::Destructive`.
    None,
}

impl RollbackStrategy {
    pub fn is_none(&self) -> bool {
        matches!(self, RollbackStrategy::None)
    }
}

/// A proposed remediation action (ADR-009 §2). Produced by the RemediationAgent,
/// evaluated by the approval gate, executed (if approved) by the executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationProposal {
    pub id: Uuid,
    pub investigation_id: Uuid,
    pub proposed_by: AgentId,
    pub category: RemediationCategory,
    /// The agent's self-assessed risk. The gate re-derives via
    /// [`RemediationProposal::enforced_risk`] and never trusts a value lower
    /// than the enforced one.
    pub risk: RiskLevel,
    pub confidence: f64,
    /// Every resource this action will touch. Length MUST be accurate — the
    /// executor rejects an action whose ARM response affects more.
    pub scope_resources: Vec<ResourceId>,
    pub arm_request: ArmWriteRequest,
    pub rollback: RollbackStrategy,
    pub rationale: String,
    /// Titles of the findings that motivated this proposal (evidence trail).
    pub evidence: Vec<String>,
    pub proposed_at: DateTime<Utc>,
}

impl RemediationProposal {
    /// The effective risk level, enforcing the ADR-009 invariants regardless of
    /// what the agent self-reported:
    /// - always-destructive categories => `Destructive`
    /// - no rollback => `Destructive`
    /// - otherwise the agent's `risk`, but never below what the invariants imply.
    pub fn enforced_risk(&self) -> RiskLevel {
        if self.category.is_always_destructive() || self.rollback.is_none() {
            return RiskLevel::Destructive;
        }
        self.risk
    }

    /// Blast radius — number of resources this action touches.
    pub fn blast_radius(&self) -> usize {
        self.scope_resources.len()
    }
}

/// Orchestrator -> RemediationAgent: synthesize remediation proposals for an
/// investigation's findings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationContext {
    pub investigation_id: Uuid,
    pub affected_resource: String,
    pub findings: Vec<Finding>,
    pub params: InvestigationParams,
}

/// RemediationAgent -> Orchestrator: the set of proposals for an investigation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemediationProposalSet {
    pub investigation_id: Uuid,
    pub proposals: Vec<RemediationProposal>,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm() -> ArmWriteRequest {
        ArmWriteRequest {
            method: "POST".to_string(),
            resource_id: "/subscriptions/s/.../sites/web".to_string(),
            api_version: "2023-01-01".to_string(),
            body: serde_json::json!({}),
        }
    }

    fn proposal(
        category: RemediationCategory,
        rollback: RollbackStrategy,
        risk: RiskLevel,
    ) -> RemediationProposal {
        RemediationProposal {
            id: Uuid::nil(),
            investigation_id: Uuid::nil(),
            proposed_by: AgentId("remediation-agent".to_string()),
            category,
            risk,
            confidence: 0.96,
            scope_resources: vec![ResourceId("web".to_string())],
            arm_request: arm(),
            rollback,
            rationale: "test".to_string(),
            evidence: vec![],
            proposed_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
        }
    }

    #[test]
    fn always_destructive_categories_override_self_reported_risk() {
        let p = proposal(
            RemediationCategory::DeleteResource,
            RollbackStrategy::PointInTimeRestore {
                resource: ResourceId("web".to_string()),
                restore_token: "t".to_string(),
            },
            RiskLevel::LowRisk, // agent lied / mis-classified
        );
        assert_eq!(p.enforced_risk(), RiskLevel::Destructive);
    }

    #[test]
    fn no_rollback_forces_destructive() {
        let p = proposal(
            RemediationCategory::RestartWorkload,
            RollbackStrategy::None,
            RiskLevel::LowRisk,
        );
        assert_eq!(p.enforced_risk(), RiskLevel::Destructive);
    }

    #[test]
    fn low_risk_with_rollback_stays_low() {
        let p = proposal(
            RemediationCategory::RestartWorkload,
            RollbackStrategy::PriorStateSnapshot {
                captured_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                snapshot: serde_json::json!({"replicas": 2}),
            },
            RiskLevel::LowRisk,
        );
        assert_eq!(p.enforced_risk(), RiskLevel::LowRisk);
    }

    #[test]
    fn blocklist_and_risk_ordering() {
        assert!(RemediationCategory::ObservabilityMutation.is_blocklisted());
        assert!(!RemediationCategory::RestartWorkload.is_blocklisted());
        assert!(RiskLevel::ReadOnly < RiskLevel::LowRisk);
        assert!(RiskLevel::HighRisk < RiskLevel::Destructive);
    }

    #[test]
    fn proposal_roundtrips_as_json() {
        let p = proposal(
            RemediationCategory::ScaleCompute,
            RollbackStrategy::PriorStateSnapshot {
                captured_at: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                snapshot: serde_json::json!({"replicas": 1}),
            },
            RiskLevel::LowRisk,
        );
        let json = serde_json::to_string(&p).unwrap();
        let back: RemediationProposal = serde_json::from_str(&json).unwrap();
        assert_eq!(back.category, RemediationCategory::ScaleCompute);
        assert_eq!(back.enforced_risk(), RiskLevel::LowRisk);
    }
}
