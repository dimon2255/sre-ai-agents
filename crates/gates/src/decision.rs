use serde::Serialize;

use sre_common::remediation::RemediationEventKind;

/// The gate's verdict on a proposal (ADR-009 §3).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum GateDecision {
    /// May execute immediately — all auto-approve conditions held and the
    /// rate-limit budget + resource lock were reserved.
    AutoApproved,
    /// Routed to humans. `approvals_needed` is 1 (HighRisk) or 2 (Destructive).
    RequiresApproval {
        approvals_needed: u8,
        reasons: Vec<String>,
    },
    /// Rejected before the gate — a blocklisted category (ObservabilityMutation).
    Rejected { reason: String },
    /// Blocked by a guardrail (kill switch, rate limit, resource lock, window).
    Blocked { reason: String },
}

impl GateDecision {
    pub fn is_auto_approved(&self) -> bool {
        matches!(self, GateDecision::AutoApproved)
    }

    /// The audit event kind this decision should be recorded as (ADR-009 §7).
    pub fn event_kind(&self) -> RemediationEventKind {
        match self {
            GateDecision::AutoApproved => RemediationEventKind::AutoApproved,
            GateDecision::RequiresApproval { .. } => RemediationEventKind::ApprovalRequested,
            GateDecision::Rejected { .. } => RemediationEventKind::Rejected,
            GateDecision::Blocked { .. } => RemediationEventKind::Blocked,
        }
    }

    /// Human-readable detail for the audit trail.
    pub fn detail(&self) -> String {
        match self {
            GateDecision::AutoApproved => "auto-approved".to_string(),
            GateDecision::RequiresApproval {
                approvals_needed,
                reasons,
            } => format!(
                "requires {approvals_needed} approval(s): {}",
                reasons.join("; ")
            ),
            GateDecision::Rejected { reason } => format!("rejected: {reason}"),
            GateDecision::Blocked { reason } => format!("blocked: {reason}"),
        }
    }
}
