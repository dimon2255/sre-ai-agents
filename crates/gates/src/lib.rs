//! Approval gate (Phase 2c, ADR-009) — the trust boundary for remediation.
//!
//! This crate is **pure policy**: it decides whether a [`RemediationProposal`]
//! may auto-execute, must be routed to one or two humans, is blocked by a
//! guardrail, or is rejected outright. It does not execute anything and depends
//! only on `sre-common` — the data lives there, the *enforcement* lives here
//! (ADR-009 §1 placement rationale).
//!
//! The security-critical invariant: the gate re-derives risk via
//! [`sre_common::remediation::RemediationProposal::enforced_risk`] and never
//! trusts a self-reported risk lower than the enforced one. An agent that
//! mis-tags a `DeleteResource` as `LowRisk` still routes to dual approval.
//!
//! [`RemediationProposal`]: sre_common::remediation::RemediationProposal

pub mod decision;
pub mod gate;
pub mod guardrails;

pub use decision::GateDecision;
pub use gate::ApprovalGate;
pub use guardrails::{resource_type_of, GuardrailConfig};
