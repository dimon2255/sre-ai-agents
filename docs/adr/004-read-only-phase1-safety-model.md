# ADR-004: Read-Only Phase 1 Safety Model

## Status

Accepted (2026-04-05)

## Context

The SRE agent platform will eventually propose and execute remediation actions against production Azure resources (restarting services, scaling replicas, rolling back deployments). Giving an automated system write access to production infrastructure carries significant risk:

- False positive root cause analysis could trigger incorrect remediation
- Untested action execution could cause cascading failures
- Lack of operational trust (the system is new, unproven)
- No approval workflow exists yet in Phase 1

We need a safety model that builds trust progressively.

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Read-only Phase 1, gated write Phase 2, auto Phase 3** | Builds trust progressively, low risk | Slower time to full automation |
| Full read-write from day 1 with approval gates | Faster to full value | Too much risk with unproven agents, approval gates are complex to build |
| Read-only forever (advisory only) | Zero risk | Limited value, never achieves MTTR reduction through automation |

## Decision

**Three-tier progressive safety model** aligned with the project phases:

### Phase 1 — Read-Only

- Agents can **only read** from Azure (KQL queries, alert fetch, resource metadata)
- RBAC: `Reader`, `Monitoring Reader`, `Log Analytics Reader` only
- No `Contributor` or write-capable roles assigned
- Output: structured findings report (JSON) — humans decide and act
- **This is an architecture decision, not a limitation.** The system must prove its judgment before getting write access.

### Phase 2 — Gated Remediation

- RemediationAgent can **propose** actions with explicit risk levels
- Every proposal goes through an **approval gate**:

```
RiskLevel::ReadOnly     → Auto-approved (queries, no-ops)
RiskLevel::LowRisk      → Auto-approve if confidence >= 0.95
RiskLevel::HighRisk     → Requires 1 human approval (Slack/Teams)
RiskLevel::Destructive  → Requires 2 human approvals
```

- **Rate limiting guardrails:**
  - Max 3 concurrent remediation actions
  - Max 2 actions per resource per hour
  - Max 10 actions per subscription per hour
  - 15-minute cooldown after any failed action
- **Rollback plans required** — every proposal must include a rollback action
- **Timeout escalation** — if no human responds within configurable timeout, escalate to next on-call tier

### Phase 3 — Progressive Autonomy

- **Auto-remediation** only when ALL criteria are met:
  - Pattern matches a previously successful remediation (similarity >= 0.90)
  - Agent confidence in root cause >= 0.95
  - LLM reasoning confidence >= 0.90
  - Risk level is at most `LowRisk`
  - Resource is on the auto-remediation allow-list
  - No other active remediation on the same resource
  - Within rate limits
- **Progressive trust model:**
  - Confidence thresholds start high (0.95) and decrease incrementally (minimum 0.85)
  - Thresholds only decrease after sustained success (N successful actions with zero failures)
  - Any failed auto-remediation immediately raises the threshold and triggers cooldown
- **Kill switch:** Global flag to disable all auto-remediation instantly

## Consequences

### What becomes easier

- **Safe Phase 1 rollout** — zero risk of automated actions causing production impact during the learning period.
- **Trust building** — operations team gains confidence in the platform's judgment before granting write access.
- **Auditability** — clear boundary between "system suggested X" and "system executed X" in the audit log.
- **Regulatory compliance** — progressive model demonstrates controlled automation for audit purposes.

### What becomes harder

- **Time to full automation** — full auto-remediation is a Phase 3 capability, likely 6+ months out.
- **Phase 2 complexity** — approval gates, Slack/Teams integration, and rate limiting add significant implementation scope.
- **Confidence calibration** — the progressive trust model requires careful tuning to avoid being either too aggressive or too conservative.
