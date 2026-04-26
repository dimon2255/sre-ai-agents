# ADR-009: Remediation Safety Model

## Status

Draft (2026-04-22). Target: Accepted before Phase 2c implementation begins.

## Context

Phase 1 deliberately shipped a **read-only** investigation platform (ADR-004) — every agent queries Azure, none mutate it. Phase 2c lifts that restriction: the platform begins proposing and, for a narrow class of low-risk actions, *executing* remediations against Azure Resource Manager.

This introduces a class of failure modes the read-only platform could not have. An automated system mutating production resources based on statistical inference of root cause is — if we are not extremely careful — indistinguishable from an adversary with the same credentials. The control plane for that credential must be at least as rigorous as what a human operator is held to.

This ADR establishes the **safety envelope** for Phase 2c: what actions may be automated, what must be gated by a human, how blast radius is bounded, how destructive actions are constrained, what evidence every action leaves behind, and what the kill-switch looks like when something goes wrong. It is a **paper artifact** — the corresponding code and types are implemented in Phase 2c.

### Why this is drafted in Phase 2a, not Phase 2c

Writing this design three weeks before the code forces the ADR to be a genuine constraint on implementation rather than a post-hoc rationalisation of what the code already does. If the Phase 2c implementation wants to deviate from this envelope, that deviation becomes an explicit ADR amendment — reviewable, auditable, refusable — rather than a silent choice made during a sprint.

### Out of scope

- Identity separation between investigation and remediation credentials — covered by ADR-011 (planned).
- Webhook authentication — covered by ADR-008 (Accepted 2026-04-21).
- LLM-driven reasoning over remediation options — Phase 3, outside this envelope.
- Remediation of non-Azure resources — not in platform scope.

## Threat Model

### STRIDE

| # | Threat | STRIDE | Scenario | Mitigation |
|---|--------|--------|----------|------------|
| 1 | Automated system executes a destructive action based on a misattributed root cause | Tampering | RemediationAgent confidence score is inflated; auto-approve gate let it through | Auto-approve is gated on `RiskLevel == LowRisk`. Destructive actions **cannot** auto-approve regardless of confidence |
| 2 | Runaway remediation cascade — one action triggers an alert, which triggers another action, etc. | Denial of service | Scale-out remediation causes new alerts, which cause more scale-out | Per-subscription rate limit; self-exclusion filter (ADR-007) already blocks the platform from investigating its own alerts; global kill-switch |
| 3 | Blast-radius expansion — action intended for one resource affects many | Elevation of privilege (effectively) | ARM API call matches more resources than intended (e.g. tag-based selector) | Every proposal declares `scope_resources: Vec<ResourceId>`. Scope > 10 resources auto-classifies as `HighRisk` |
| 4 | Destructive action executed without a rollback plan | Tampering | Agent proposes a remediation whose effects cannot be undone | `RollbackStrategy::None` → action is auto-classified `Destructive`, which cannot auto-approve. Even with dual approval, execution records the irreversibility |
| 5 | Compromised platform credentials used to mutate resources outside the remediation envelope | Elevation of privilege | MI token leaked → attacker uses ARM write scope for unrelated purposes | Scoped MI with per-resource-type RBAC (ADR-011). Today's `sre-agents-identity` is too broad for this phase |
| 6 | Audit trail tampering — after the fact, who-approved-what cannot be proven | Repudiation | event_log rows modified or deleted to hide an action | `event_log` rows are insert-only (enforced at the PgStore level — no UPDATE/DELETE queries exist). Postgres row-level security layered on in Phase 2c |
| 7 | Approval-gate bypass — a proposal is marked approved without a human having approved it | Spoofing | Bug in sre-gates allows self-approval; or Slack-message forgery | Separate `sre-gates` crate (Phase 2c) runs in-process but with explicit trust boundary; approval messages carry a signed payload; approvals emitted to `event_log` only from the gate crate |
| 8 | Race between auto-execution and manual operator intervention | Tampering | Operator is mid-way through manually fixing X, platform executes its own remediation for X | Per-resource mutual-exclusion lock (`workflows` row level) + time-of-day suppression window configurable per subscription |

### MITRE ATT&CK TTPs the platform must not enable

Even though the platform is *defensive*, a compromised or misbehaving remediation layer can emulate adversary behavior. These TTPs are the design anti-goals:

| Technique | ID | Anti-goal |
|-----------|-----|-----------|
| Valid Accounts | [T1078](https://attack.mitre.org/techniques/T1078/) | Platform MI must not be usable for actions outside the remediation envelope. Scoped RBAC enforces this at the Azure side (ADR-011). |
| Account Manipulation | [T1098](https://attack.mitre.org/techniques/T1098/) | Platform may never mutate identity resources (Azure AD, service principals, role assignments). RemediationProposal category `IdentityMutation` is **always** `Destructive` and requires triple-party approval (operator + security on-call + second operator). |
| Data Destruction | [T1485](https://attack.mitre.org/techniques/T1485/) | Delete operations on storage accounts, databases, key vaults are always `Destructive`. Dual approval; rollback plan must include point-in-time-restore / soft-delete identifiers. |
| Account Access Removal | [T1531](https://attack.mitre.org/techniques/T1531/) | Platform may not disable user accounts or remove role assignments. Category `AccessRemoval` is always `Destructive`. |
| Impair Defenses | [T1562](https://attack.mitre.org/techniques/T1562/) | Platform may not mutate diagnostic settings, Log Analytics workspace configuration, alert rules, Defender policies, or the platform's own observability config. Category `ObservabilityMutation` is blocklisted — no risk tier; proposals of this shape are rejected before they reach the gate. |

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Risk-tiered with conservative auto-approve** (chosen) | Captures the 80/20 — common safe remediations (restart a pod, bump replica count, retry a transient failure) are automated, the long tail goes to humans. Blast radius capped by explicit scope + rollback requirements | More complex than always-gate; relies on the risk classifier being correct. The classifier is the attack surface — mitigated by making destructive-inducing categories non-overridable |
| B. Always require human approval (no auto-execute) | Zero risk of automated mutation gone wrong; simplest possible trust model | Defeats ~70% of the value of Phase 2c. For a 2am alert where the remediation is "restart the deployment," waking a human for a hands-off action is net-negative for MTTR |
| C. Fully autonomous with retroactive audit | Fastest MTTR possible | Unacceptable. Any data-destruction remediation with a wrong-confidence signal permanently destroys state. No rollback for "the database got dropped." |
| D. Two-tier: propose-only for Phase 2c, auto-execute only in Phase 3 | Buys time for the trust model to mature under real traffic before enabling auto-execute | Delays value; but if Phase 2c reveals that the risk classifier is systematically mis-calibrated, this becomes the fallback. Keep as an escape hatch |

## Decision

Adopt **Option A** — risk-tiered auto-approve with a narrow auto-execute envelope and a dominant human-in-the-loop path.

### 1. RiskLevel enum (in `sre-common`)

```rust
/// Classification of a proposed remediation action. Used by the approval
/// gate to decide whether the action can auto-approve or must be routed
/// to a human operator. Placed in sre-common because every crate that
/// constructs or routes a RemediationProposal carries this tag.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    /// No side effects on Azure state. Diagnostic or informational only
    /// (e.g. "run a query, return the result"). Auto-approved unconditionally.
    ReadOnly,

    /// Reversible, bounded, non-data-touching. Example: scale replica
    /// count ±1, restart a container. Auto-approve iff confidence >= 0.95
    /// AND scope <= 1 resource AND rate-limit OK.
    LowRisk,

    /// Configuration or capacity changes with a larger blast radius.
    /// Example: scale +3 replicas, rotate a non-critical certificate.
    /// Always requires single human approval. Never auto-approves.
    HighRisk,

    /// Irreversible or identity/data/observability touching. Example:
    /// delete a resource, rotate a KMS key, modify a diagnostic setting.
    /// Always requires **dual approval** from distinct humans. Never
    /// auto-approves. Must carry a rollback plan; actions without
    /// rollback automatically fall into this tier regardless of category.
    Destructive,
}
```

Placement rationale (Software Architect, 2026-04-21): RiskLevel travels inside `AgentPayload::RemediationProposal(...)`. If placed in `sre-gates`, that crate would have to depend on `sre-common` *and* `sre-common` would have to depend on `sre-gates` for typing the payload — a cycle. `sre-common` owns the data; `sre-gates` owns the enforcement.

### 2. RemediationProposal shape

```rust
pub struct RemediationProposal {
    pub id: Uuid,
    pub investigation_id: Uuid,
    pub proposed_by: AgentId,
    pub category: RemediationCategory,
    pub risk: RiskLevel,
    pub confidence: f64,
    /// Every resource this action will touch. Length MUST be accurate —
    /// an action that mutates more than this will be rejected at
    /// execution time (post-mortem check against ARM response).
    pub scope_resources: Vec<ResourceId>,
    pub arm_request: ArmWriteRequest,   // the actual mutation
    pub rollback: RollbackStrategy,     // how to undo — required
    pub rationale: String,              // human-readable why
    pub evidence: Vec<FindingId>,       // findings that motivated this
    pub proposed_at: DateTime<Utc>,
}

pub enum RollbackStrategy {
    /// Capture prior state, execute action, retain prior state for reversal.
    /// The default. Works for most config-change remediations.
    PriorStateSnapshot { captured_at: DateTime<Utc>, snapshot: serde_json::Value },

    /// Point-in-time restore via an Azure-native mechanism (geo-restore,
    /// storage soft-delete, etc.) Must carry the restore identifier.
    PointInTimeRestore { resource: ResourceId, restore_token: String },

    /// No rollback possible. Forces `RiskLevel::Destructive`. Requires
    /// explicit acknowledgement from the approver that the action is
    /// irreversible.
    None,
}

pub enum RemediationCategory {
    RestartWorkload,
    ScaleCompute,
    RotateCertificate,
    PurgeCache,
    ClearQueue,
    DeleteResource,          // always Destructive
    IdentityMutation,        // always Destructive
    AccessRemoval,           // always Destructive
    ObservabilityMutation,   // BLOCKLISTED — rejected before gate
}
```

### 3. Auto-approve policy

A proposal auto-approves iff **all** of the following hold:

1. `risk == RiskLevel::ReadOnly` **or** (`risk == RiskLevel::LowRisk` **and** `confidence >= 0.95` **and** `scope_resources.len() <= 1`)
2. `rollback != RollbackStrategy::None`
3. Per-subscription rate limit has budget (default: 5 auto-approvals / hour / subscription)
4. Per-resource-type rate limit has budget (default: 3 auto-approvals / hour / resource type)
5. Global kill-switch is disengaged (`SRE__REMEDIATION__KILL_SWITCH=false`)
6. No mutual-exclusion lock held by another proposal for any resource in `scope_resources`
7. Current time is inside the configured auto-approve window for the target subscription (defaults to 24/7; operators may restrict to business-hours only)
8. `category != RemediationCategory::ObservabilityMutation` (defensive — should never reach the gate)

Any failure → route to human approval. `Destructive` routes to the **two-humans-required** flow (no single human can approve).

### 4. Guardrails

| Guardrail | Default | Override |
|-----------|---------|----------|
| Per-subscription auto-approve rate | 5/hour | `SRE__REMEDIATION__RATE_LIMITS__<SUB_ID>` |
| Per-resource-type auto-approve rate | 3/hour | same prefix |
| Global kill-switch | off (remediation enabled) | env var or operator API |
| Blast-radius cap for auto-approve | 1 resource | non-overridable — higher requires human |
| Blocklisted categories | `ObservabilityMutation`, `IdentityMutation`, `AccessRemoval` for auto | non-overridable |
| Business-hours window | 24/7 | per-subscription config |

### 5. Rollback contract

Every `RemediationProposal` carries a `rollback: RollbackStrategy` field. It is **not optional**. Three possible values (defined in §2 above). Proposals where the agent cannot determine a rollback strategy must emit `RollbackStrategy::None`, which forces `RiskLevel::Destructive`. This pathway is compiler-enforced, not policy-enforced — there is no "forgot to set the field" failure mode.

Execution records include the rollback artefact (snapshot content, restore token) in `event_log.payload_json` so an operator can roll back manually if the platform is down.

### 6. Identity separation (forward-reference ADR-011)

Remediation write-path **must not** use the same managed identity as read-path investigation. ADR-011 defines:

- A distinct MI (`id-sre-agents-remediation`) with narrow per-resource-type RBAC roles.
- Short-lived tokens (≤1 hour) scoped to each remediation execution, not held by the platform long-term.
- Separate audit trail via Azure Activity Log on the remediation MI so post-hoc analysis can distinguish platform reads from platform writes.

Phase 2a/2b may continue with the `sre-agents-identity` (read-only ARM scope); Phase 2c is blocked on ADR-011 landing first.

### 7. Audit requirements

Every lifecycle event for a proposal is an insert-only row in `event_log`:

| Event type | Emitted by | Payload |
|-----------|-----------|---------|
| `remediation_proposed` | RemediationAgent | Full `RemediationProposal` |
| `remediation_auto_approved` | `sre-gates` | Proposal ID + which guardrails were checked |
| `remediation_approval_requested` | `sre-gates` | Proposal ID + approver list + Slack message ID |
| `remediation_approved` | `sre-gates` | Proposal ID + approver identity + signed payload from Slack |
| `remediation_rejected` | `sre-gates` | Proposal ID + rejector identity + reason |
| `remediation_executing` | Executor | Proposal ID + start timestamp |
| `remediation_executed` | Executor | Proposal ID + ARM response + actual scope (post-mortem check) |
| `remediation_failed` | Executor | Proposal ID + error |
| `remediation_rolled_back` | Executor | Proposal ID + rollback method + result |

These travel in `AgentPayload` as new variants added behind the existing `#[non_exhaustive]` attribute on the enum. No breaking change to sre-common.

`workflows.state` enum gains new variants: `Proposing`, `AwaitingApproval`, `Executing`, `RolledBack`. This is a Phase 2c migration (`ALTER TYPE workflow_state ADD VALUE ...`), not a Phase 2a concern.

## Consequences

### Positive

- Platform has a clear, written, reviewable boundary between what may be automated and what must not.
- Risk classifier is the attack surface, but it is a **narrow** attack surface — defeating it requires the agent to both mis-classify *and* the rollback plan to be fabricated.
- Destructive categories (identity, access, observability, data deletion) are compiler-enforced off the auto-approve path.
- Audit trail is insert-only, indexed by correlation_id (ADR-012), and machine-queryable for post-mortem analysis.
- RiskLevel type placement (`sre-common`) lets agents carry risk context end-to-end without circular dependencies.

### Negative

- The risk classifier is platform logic, not Azure-side enforcement. If compromised, an attacker who can emit proposals could attempt to mis-tag destructive actions as LowRisk. Mitigated by: (a) the auto-approve gate double-checks category against blocklist; (b) rate limits bound damage; (c) scoped MI (ADR-011) caps blast radius at the Azure side.
- Human-approval round-trip is inherently latent (30s–minutes for operator to respond on Slack). For genuinely 2am incidents this reduces the MTTR win vs. a hypothetical full-auto system. Accepted trade-off.
- RemediationProposal payload is larger than read-only AgentPayload variants, widening the `mpsc` / (future) NATS message size. Acceptable under current 64 KB body limit.
- Two-human dual-approval flow adds implementation complexity in `sre-gates`. Phase 2c cost.

### Follow-ups and open questions

- **Blast-radius post-mortem check**: the execution path verifies that `arm_response.affected_resources ⊆ scope_resources`. What happens on mismatch — rollback everything, or halt further execution? Proposed: **automatic rollback + escalation-to-human**. Confirm in Phase 2c implementation review.
- **Approval-message signing**: Slack interactive messages carry a signing secret per request. Document the exact verification path in a Phase 2c companion ADR.
- **Override window**: does a human operator need a "break-glass" mechanism to execute a `Destructive` action without dual approval? Proposed: **no** — break-glass is the human running `az` directly, outside the platform.

## References

- [ADR-004: Read-Only Phase 1 Safety Model](004-read-only-phase1-safety-model.md) — the precursor envelope this ADR expands.
- [ADR-008: Webhook Authentication](008-webhook-authentication.md) — trust boundary on the ingest side. This ADR extends it to the egress (write) side.
- ADR-011: Identity Separation for Remediation (planned) — scoped MI with per-resource-type RBAC.
- [ADR-012: Schema Type Modernization](012-schema-type-modernization.md) — `workflow_state` enum this ADR proposes extending.
- MITRE ATT&CK Enterprise Matrix — referenced TTP IDs above.
- [Google SRE Book — Chapter 6: Monitoring Distributed Systems](https://sre.google/sre-book/monitoring-distributed-systems/) — background on automation trust boundaries.
