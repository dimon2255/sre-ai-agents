use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use uuid::Uuid;

use sre_common::remediation::{RemediationProposal, RiskLevel};

use crate::decision::GateDecision;
use crate::guardrails::{in_business_hours, resource_type_of, GuardrailConfig, RateLimiter};

struct GateState {
    rate: RateLimiter,
    /// Resource ids currently held by an in-flight remediation (mutual exclusion).
    locks: HashSet<String>,
    /// proposal id -> distinct approvers collected so far.
    pending: HashMap<Uuid, HashSet<String>>,
}

/// The approval gate (ADR-009 §3). Evaluates proposals against the auto-approve
/// policy and guardrails. Stateful: it holds the rate-limit window, the
/// per-resource mutual-exclusion locks, and the human-approval tally. Shared
/// across investigations as `Arc<ApprovalGate>`; the critical sections are O(1)
/// and never cross an await, so a `std::sync::Mutex` is correct.
pub struct ApprovalGate {
    config: GuardrailConfig,
    state: Mutex<GateState>,
}

impl ApprovalGate {
    pub fn new(config: GuardrailConfig) -> Self {
        Self {
            config,
            state: Mutex::new(GateState {
                rate: RateLimiter::hourly(),
                locks: HashSet::new(),
                pending: HashMap::new(),
            }),
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(GuardrailConfig::default())
    }

    pub fn kill_switch_engaged(&self) -> bool {
        self.config.kill_switch
    }

    /// Evaluate a proposal. On `AutoApproved`, this **atomically reserves** the
    /// per-subscription + per-resource-type rate-limit budget and acquires the
    /// resource locks, so concurrent auto-approvals can't race past the limits.
    /// Call [`ApprovalGate::release`] after execution/rollback.
    ///
    /// Uses the configured `auto_approve_min_confidence` as the LowRisk floor.
    /// For a trust-adjusted floor (Phase 3 progressive trust, ADR-017) use
    /// [`ApprovalGate::evaluate_with_confidence_floor`].
    pub fn evaluate(
        &self,
        proposal: &RemediationProposal,
        subscription_id: &str,
        now: DateTime<Utc>,
    ) -> GateDecision {
        self.evaluate_with_confidence_floor(
            proposal,
            subscription_id,
            now,
            self.config.auto_approve_min_confidence,
        )
    }

    /// Like [`ApprovalGate::evaluate`], but with an explicit LowRisk auto-approve
    /// confidence floor. The progressive-trust model (ADR-017) passes an earned
    /// floor (down to 0.85) for trusted categories, or an unreachable floor
    /// (> 1.0) to suspend auto-approval during a cooldown — which routes the
    /// proposal to human approval through the gate's normal LowRisk path. All
    /// other policy (risk routing, blocklist, kill switch, blast radius, rate
    /// limits, locks) is unchanged: trust only modulates the LowRisk confidence
    /// bar; HighRisk/Destructive are never auto-approved.
    pub fn evaluate_with_confidence_floor(
        &self,
        proposal: &RemediationProposal,
        subscription_id: &str,
        now: DateTime<Utc>,
        min_confidence: f64,
    ) -> GateDecision {
        // Risk is re-derived here — the gate never trusts a self-reported value
        // lower than the enforced one (ADR-009 attack-surface mitigation).
        let risk = proposal.enforced_risk();

        // 0. Blocklist — rejected before any other consideration.
        if proposal.category.is_blocklisted() {
            return GateDecision::Rejected {
                reason: "category is blocklisted (ObservabilityMutation)".into(),
            };
        }

        // 1. Global kill switch.
        if self.config.kill_switch {
            return GateDecision::Blocked {
                reason: "global remediation kill switch engaged".into(),
            };
        }

        // 2. Risk-based routing.
        match risk {
            RiskLevel::ReadOnly => {} // proceed to guardrail reservation
            RiskLevel::LowRisk => {
                let mut reasons = Vec::new();
                if proposal.confidence < min_confidence {
                    reasons.push(format!(
                        "confidence {:.2} < required {:.2}",
                        proposal.confidence, min_confidence
                    ));
                }
                if proposal.blast_radius() > self.config.max_auto_approve_blast_radius {
                    reasons.push(format!(
                        "blast radius {} > max {}",
                        proposal.blast_radius(),
                        self.config.max_auto_approve_blast_radius
                    ));
                }
                if !reasons.is_empty() {
                    return GateDecision::RequiresApproval {
                        approvals_needed: 1,
                        reasons,
                    };
                }
            }
            RiskLevel::HighRisk => {
                return GateDecision::RequiresApproval {
                    approvals_needed: 1,
                    reasons: vec!["HighRisk always requires a single human approval".into()],
                }
            }
            RiskLevel::Destructive => {
                return GateDecision::RequiresApproval {
                    approvals_needed: 2,
                    reasons: vec![
                        "Destructive requires dual approval from distinct operators".into()
                    ],
                }
            }
        }

        // 3. Auto-approve window.
        if self.config.business_hours_only && !in_business_hours(now) {
            return GateDecision::Blocked {
                reason: "outside the configured auto-approve window".into(),
            };
        }

        // 4. Guardrail reservation (rate limits + mutual exclusion), atomic.
        let rtype = resource_type_of(&proposal.arm_request.resource_id);
        let sub_key = format!("sub:{subscription_id}");
        let rtype_key = format!("rtype:{rtype}");

        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());

        for r in &proposal.scope_resources {
            if st.locks.contains(&r.0) {
                return GateDecision::Blocked {
                    reason: format!(
                        "resource {} is locked by another in-flight remediation",
                        r.0
                    ),
                };
            }
        }
        if st.rate.count(&sub_key, now) >= self.config.per_subscription_hourly_limit {
            return GateDecision::Blocked {
                reason: "per-subscription auto-approve rate limit reached".into(),
            };
        }
        if st.rate.count(&rtype_key, now) >= self.config.per_resource_type_hourly_limit {
            return GateDecision::Blocked {
                reason: format!("per-resource-type ({rtype}) auto-approve rate limit reached"),
            };
        }

        // Reserve.
        st.rate.record(&sub_key, now);
        st.rate.record(&rtype_key, now);
        for r in &proposal.scope_resources {
            st.locks.insert(r.0.clone());
        }
        GateDecision::AutoApproved
    }

    /// Record a human approval. Returns the number of **distinct** approvers so
    /// far — the caller compares against `approvals_needed` (1 for HighRisk, 2
    /// for Destructive). The same operator approving twice does not count twice.
    pub fn approve(&self, proposal_id: Uuid, approver: &str) -> usize {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let set = st.pending.entry(proposal_id).or_default();
        set.insert(approver.to_string());
        set.len()
    }

    /// Acquire the resource locks for a (human-approved) proposal about to
    /// execute. Returns `false` if any resource is already locked. Auto-approved
    /// proposals already hold their locks via `evaluate`; this is the path the
    /// executor uses for the human-approved flow.
    pub fn acquire_locks(&self, proposal: &RemediationProposal) -> bool {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if proposal
            .scope_resources
            .iter()
            .any(|r| st.locks.contains(&r.0))
        {
            return false;
        }
        for r in &proposal.scope_resources {
            st.locks.insert(r.0.clone());
        }
        true
    }

    /// Release the locks and approval tally for a proposal after the executor
    /// finishes (success, failure, or rollback).
    pub fn release(&self, proposal: &RemediationProposal) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        for r in &proposal.scope_resources {
            st.locks.remove(&r.0);
        }
        st.pending.remove(&proposal.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::messages::AgentId;
    use sre_common::remediation::{
        ArmWriteRequest, RemediationCategory, RemediationProposal, ResourceId, RollbackStrategy,
    };

    fn now() -> DateTime<Utc> {
        // 12:00 UTC — inside business hours for the window tests.
        DateTime::<Utc>::from_timestamp(12 * 3600, 0).unwrap()
    }

    fn snapshot() -> RollbackStrategy {
        RollbackStrategy::PriorStateSnapshot {
            captured_at: now(),
            snapshot: serde_json::json!({"x": 1}),
        }
    }

    fn proposal(
        category: RemediationCategory,
        risk: RiskLevel,
        confidence: f64,
        rollback: RollbackStrategy,
        scope: Vec<&str>,
    ) -> RemediationProposal {
        RemediationProposal {
            id: Uuid::new_v4(),
            investigation_id: Uuid::nil(),
            proposed_by: AgentId("remediation-agent".to_string()),
            category,
            risk,
            confidence,
            scope_resources: scope
                .into_iter()
                .map(|s| ResourceId(s.to_string()))
                .collect(),
            arm_request: ArmWriteRequest {
                method: "POST".to_string(),
                resource_id:
                    "/subscriptions/s/resourceGroups/rg/providers/Microsoft.App/containerApps/web"
                        .to_string(),
                api_version: "2024-03-01".to_string(),
                body: serde_json::json!({}),
            },
            rollback,
            rationale: "test".to_string(),
            evidence: vec![],
            proposed_at: now(),
        }
    }

    fn low_risk_ok() -> RemediationProposal {
        proposal(
            RemediationCategory::RestartWorkload,
            RiskLevel::LowRisk,
            0.97,
            snapshot(),
            vec!["web"],
        )
    }

    #[test]
    fn low_risk_high_confidence_single_scope_auto_approves() {
        let gate = ApprovalGate::with_defaults();
        assert_eq!(
            gate.evaluate(&low_risk_ok(), "sub-1", now()),
            GateDecision::AutoApproved
        );
    }

    #[test]
    fn low_confidence_routes_to_single_approval() {
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::RestartWorkload,
            RiskLevel::LowRisk,
            0.80,
            snapshot(),
            vec!["web"],
        );
        match gate.evaluate(&p, "sub-1", now()) {
            GateDecision::RequiresApproval {
                approvals_needed, ..
            } => assert_eq!(approvals_needed, 1),
            other => panic!("expected RequiresApproval, got {other:?}"),
        }
    }

    #[test]
    fn blast_radius_over_one_routes_to_approval() {
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::RestartWorkload,
            RiskLevel::LowRisk,
            0.99,
            snapshot(),
            vec!["web", "api"],
        );
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::RequiresApproval {
                approvals_needed: 1,
                ..
            }
        ));
    }

    #[test]
    fn high_risk_always_single_approval() {
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::RestartWorkload,
            RiskLevel::HighRisk,
            0.99,
            snapshot(),
            vec!["web"],
        );
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::RequiresApproval {
                approvals_needed: 1,
                ..
            }
        ));
    }

    #[test]
    fn destructive_requires_dual_approval() {
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::DeleteResource,
            RiskLevel::Destructive,
            0.99,
            snapshot(),
            vec!["web"],
        );
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::RequiresApproval {
                approvals_needed: 2,
                ..
            }
        ));
    }

    #[test]
    fn mis_tagged_destructive_category_still_routes_to_dual_approval() {
        // Agent lies: DeleteResource self-reported as LowRisk. enforced_risk()
        // must override to Destructive => dual approval.
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::DeleteResource,
            RiskLevel::LowRisk,
            0.99,
            snapshot(),
            vec!["web"],
        );
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::RequiresApproval {
                approvals_needed: 2,
                ..
            }
        ));
    }

    #[test]
    fn no_rollback_forces_dual_approval() {
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::RestartWorkload,
            RiskLevel::LowRisk,
            0.99,
            RollbackStrategy::None,
            vec!["web"],
        );
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::RequiresApproval {
                approvals_needed: 2,
                ..
            }
        ));
    }

    #[test]
    fn blocklisted_category_is_rejected() {
        let gate = ApprovalGate::with_defaults();
        let p = proposal(
            RemediationCategory::ObservabilityMutation,
            RiskLevel::LowRisk,
            0.99,
            snapshot(),
            vec!["web"],
        );
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::Rejected { .. }
        ));
    }

    #[test]
    fn kill_switch_blocks_everything() {
        let gate = ApprovalGate::new(GuardrailConfig {
            kill_switch: true,
            ..Default::default()
        });
        assert!(matches!(
            gate.evaluate(&low_risk_ok(), "sub-1", now()),
            GateDecision::Blocked { .. }
        ));
    }

    #[test]
    fn per_resource_type_rate_limit_blocks_after_budget() {
        // Default per-resource-type limit is 3. Distinct resources avoid the
        // mutual-exclusion lock so we exercise the rate limit specifically.
        let gate = ApprovalGate::with_defaults();
        for i in 0..3 {
            // Each gets a unique single-resource scope (avoids the lock).
            let mut p = proposal(
                RemediationCategory::RestartWorkload,
                RiskLevel::LowRisk,
                0.99,
                snapshot(),
                vec![],
            );
            p.scope_resources = vec![ResourceId(format!("web-{i}"))];
            assert_eq!(
                gate.evaluate(&p, "sub-1", now()),
                GateDecision::AutoApproved,
                "iter {i}"
            );
        }
        // 4th hits the per-resource-type limit (all share Microsoft.App/containerApps).
        let mut p = low_risk_ok();
        p.scope_resources = vec![ResourceId("web-4".to_string())];
        assert!(matches!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::Blocked { .. }
        ));
    }

    #[test]
    fn mutual_exclusion_blocks_second_action_on_same_resource() {
        let gate = ApprovalGate::with_defaults();
        assert_eq!(
            gate.evaluate(&low_risk_ok(), "sub-1", now()),
            GateDecision::AutoApproved
        );
        // Same resource "web" still locked → blocked.
        assert!(matches!(
            gate.evaluate(&low_risk_ok(), "sub-1", now()),
            GateDecision::Blocked { .. }
        ));
    }

    #[test]
    fn release_frees_the_lock() {
        let gate = ApprovalGate::with_defaults();
        let p = low_risk_ok();
        assert_eq!(
            gate.evaluate(&p, "sub-1", now()),
            GateDecision::AutoApproved
        );
        gate.release(&p);
        // After release, a fresh proposal on "web" can auto-approve again
        // (rate budget for one resource-type still has room: 1 used of 3).
        let p2 = low_risk_ok();
        assert_eq!(
            gate.evaluate(&p2, "sub-1", now()),
            GateDecision::AutoApproved
        );
    }

    #[test]
    fn dual_approval_needs_two_distinct_operators() {
        let gate = ApprovalGate::with_defaults();
        let id = Uuid::new_v4();
        assert_eq!(gate.approve(id, "alice"), 1);
        assert_eq!(gate.approve(id, "alice"), 1); // same operator, still 1
        assert_eq!(gate.approve(id, "bob"), 2); // distinct → 2
    }

    #[test]
    fn outside_business_hours_blocks_when_window_enabled() {
        let gate = ApprovalGate::new(GuardrailConfig {
            business_hours_only: true,
            ..Default::default()
        });
        let midnight = DateTime::<Utc>::from_timestamp(0, 0).unwrap(); // 00:00 UTC
        assert!(matches!(
            gate.evaluate(&low_risk_ok(), "sub-1", midnight),
            GateDecision::Blocked { .. }
        ));
    }
}
