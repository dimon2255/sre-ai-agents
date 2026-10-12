# ADR-011: Identity Separation for Remediation

## Status

Accepted (2026-06-20). Forward-referenced by ADR-009 §6 as the precondition for
Phase 2c write operations. The code seam exists; the Azure-side identity is a
deploy-time infra step (documented here, not yet applied).

## Context

Phase 1 and 2a/2b are **read-only** against Azure: every query uses one
user-assigned managed identity attached to the Container App (called
`sre-agents-identity` throughout this ADR), which carries read scopes (Log
Analytics Reader, Monitoring Reader). Phase 2c introduces the first *write* path — the
`AzureArmClient` issues PATCH/POST/DELETE against ARM to execute approved
remediations.

ADR-009 §6 states the invariant plainly: **the remediation write-path must not
use the same managed identity as read-path investigation.** An automated system
that can both read telemetry *and* mutate production with one identity is, if
compromised, indistinguishable from an adversary with that identity. The blast
radius of a leaked or misbehaving credential must be bounded at the Azure side,
not only by platform-internal policy (the gate is platform logic; a scoped MI is
Azure-enforced).

This ADR defines that separation.

## Decision

### 1. A distinct managed identity for the write path

Provision `id-sre-agents-remediation`, separate from `sre-agents-identity`:

- **Read path** (`sre-agents-identity`): Log Analytics Reader, Monitoring Reader.
  Unchanged. Used by `AzureQueryClient` (Logs/Metrics/Trace/Infra agents).
- **Write path** (`id-sre-agents-remediation`): **narrow, per-resource-type**
  RBAC role assignments matching only the remediation categories the platform
  is allowed to perform. Used by `AzureArmClient` (the executor).

The write identity gets **no** read scopes and **no** broad `Contributor`. Its
role assignments are the minimal set for the auto-approvable categories:

| Category | ARM action | Minimal role |
|----------|-----------|--------------|
| `RestartWorkload` | `Microsoft.App/containerApps/.../restart` | a custom role granting only the restart action on container apps |
| `ScaleCompute` | `Microsoft.App/containerApps write` (scale block) | a custom role scoped to the `scale` property |
| `ClearQueue` / `PurgeCache` | resource-specific data actions | resource-scoped custom roles |

`DeleteResource`, `IdentityMutation`, `AccessRemoval`, `ObservabilityMutation`
are **never** granted to the write identity — they are `Destructive`/blocklisted
in ADR-009 and the platform must be incapable of them at the Azure layer even if
the gate is bypassed.

### 2. Short-lived, per-execution tokens

The write identity's tokens are acquired **per execution**, scoped to the
management endpoint, and not held long-term. The `ArmWriteClient` trait already
takes a `TokenProvider`, so the executor can be handed a provider that mints a
fresh ≤1h token for each remediation rather than caching one.

### 3. Separate audit trail

Azure Activity Log entries for `id-sre-agents-remediation` are distinct from the
read identity's, so post-hoc analysis can cleanly separate "the platform read
telemetry" from "the platform mutated a resource". This complements the
platform-internal insert-only `event_log` audit (ADR-009 §7) — two independent
records of every write.

### Code seam (already in place)

The Rust code is written to honor this separation with a one-line wiring change
once the identity exists:

- `ArmWriteClient` (the write interface) is a trait, with `AzureArmClient`
  taking an `Arc<dyn TokenProvider>` — **independent** of the `AzureQueryClient`
  read credential.
- `bootstrap()` constructs the `AzureArmClient` from a credential; today it
  reuses the read credential (`sre-agents-identity`) because the remediation MI is
  not yet provisioned. When it is, the change is to pass a *second* credential
  (the remediation MI's `TokenProvider`) to `AzureArmClient::new` — no executor,
  gate, or agent code changes.

This is the deliberate "paper-and-seam-first" approach: the code boundary is
real and tested (the executor never touches the read client), and the remaining
work is Terraform + a credential swap, not a refactor.

## Consequences

### Positive

- A leaked/misbehaving write identity cannot read telemetry or perform
  destructive categories — bounded at the Azure RBAC layer, not just platform
  policy.
- The gate (platform logic) and the scoped MI (Azure enforcement) are
  **defense in depth**: defeating remediation safety requires defeating both.
- Clean Activity-Log separation of platform reads vs. platform writes.

### Negative

- Operational cost: a second identity + custom RBAC roles to author and
  maintain. Each new auto-approvable category needs a corresponding narrow role.
- Until the MI is provisioned, the write path reuses `sre-agents-identity` — so
  **remediation must remain disabled in staging/production** (`remediation.enabled
  = false`, the default) until ADR-011's Terraform lands. The code is safe to
  ship disabled; enabling it before the scoped MI exists would violate this ADR.

### Follow-ups (infra, not in this PR)

- Terraform: `azurerm_user_assigned_identity.remediation` + custom role
  definitions + scoped role assignments.
- A `TokenProvider` implementation that mints per-execution tokens for the
  remediation MI, wired into `bootstrap()` behind the `remediation.enabled` flag.
- `workflows.state` ENUM gains `Proposing` / `AwaitingApproval` / `Executing` /
  `RolledBack` (ADR-009 §7) — a corrective migration, tracked separately; the
  insert-only `event_log` is the authoritative remediation record in the
  meantime.

## References

- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — §6 forward-references this ADR; §1 the RiskLevel placement.
- [ADR-004: Read-Only Phase 1 Safety Model](004-read-only-phase1-safety-model.md) — the read-only precursor.
- [ADR-007: CI/CD, Containerization, ACA Deployment](007-cicd-containerization-aca-deployment.md) — the Container Apps deployment the read identity is attached to.
