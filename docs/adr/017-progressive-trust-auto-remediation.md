# ADR-017: Progressive Trust + Auto-Remediation

## Status

Accepted (2026-06-20). Third increment of the Phase 3 intelligence layer — turns
the Phase 2c gated-remediation pipeline into one that *earns* autonomy.

## Context

Phase 2c (ADR-009) shipped the full propose → gate → execute → verify → rollback
pipeline with a **fixed** auto-approve policy: a LowRisk action auto-executes only
at confidence ≥ 0.95, blast-radius ≤ 1, guardrails OK; everything else needs a
human. That floor is static — a `RestartWorkload` that has succeeded a thousand
times is treated exactly like one the platform has never run. The Phase 3 goal is
**trusted automated response**: let the platform lower the bar for actions that
have *proven themselves*, while keeping a hard backstop for anything risky or
newly-misbehaving.

The Phase 3 architecture diagram specifies this precisely in its *Progressive
Trust Model* panel:

```
New action type  → threshold 0.95
  10 consecutive successes → 0.93
  25 → 0.90 ; 50 → 0.87 ; 100 → 0.85 (minimum)
  1 failure → reset to 0.95 + cooldown (15 / 30 / 60 min by tier)
```

The constraints from the existing design are non-negotiable:

- The approval gate (`sre-gates`) is the trust boundary (ADR-009 §1). Trust must
  *modulate* its decision, not bypass it. Risk routing, the blocklist, the kill
  switch, blast-radius, rate limits, and locks all still apply.
- HighRisk/Destructive must **never** auto-approve, no matter how trusted a
  category becomes. Progressive trust is strictly a LowRisk-floor concern.
- The mis-tagged-risk defense (`enforced_risk()`, ADR-009) must remain intact.
- Deterministic, offline testing (injected clock, like the guardrail
  `RateLimiter`).

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. A `TrustModel` in `sre-gates` that produces a per-category confidence floor; the executor passes it to `evaluate_with_confidence_floor`; outcomes feed back** (chosen) | Trust *modulates* the existing gate, never bypasses it — all other policy is unchanged. Floor is bounded `[0.85, 0.95]`; HighRisk/Destructive routing is untouched. Cooldown is expressed as an unreachable floor, so suspension reuses the gate's normal human-approval path with zero special-casing. Pure policy + injected clock = deterministic tests. Opt-in; off → identical Phase 2c behavior. | A second stateful policy object (alongside the gate's rate/lock state). In-memory trust state is per-process (resets on restart) until persisted. |
| B. Bake the trust state machine *into* `ApprovalGate` | One object. | Conflates two concerns (point-in-time policy vs. longitudinal trust); bloats the gate; harder to test in isolation; the gate stops being a pure function of (proposal, now). |
| C. Trust in `sre-intelligence` beside the `LearningLoop` `ConfidenceTracker` | Co-locates "learning". | The trust floor is a *gating* decision; putting it outside `sre-gates` splits the trust boundary across crates and forces the gate to depend on the intelligence layer. The `ConfidenceTracker` (cumulative success-rate, for insight/RAG) and the `TrustModel` (consecutive-success threshold machine, for gating) are deliberately distinct — different math, different consumer. |
| D. Auto-execute when trusted, skipping the gate entirely | Simplest "autonomy". | Bypasses rate limits, locks, kill switch, blast-radius — unacceptable. The gate must always decide. |

## Decision

Adopt **Option A**.

### 1. `TrustModel` (`sre-gates::trust`)

Per-`RemediationCategory` state: `consecutive_successes` + an optional
`cooldown_until`. Pure policy, `std::sync::Mutex`, **injected clock**.

- `threshold_for(category, now) -> Option<f64>` — the effective LowRisk
  auto-approve floor, clamped to `[0.85, 0.95]`. **`None` when in cooldown**
  (auto-remediation suspended).
- `record(category, success, now)` — success increments consecutive successes
  (ratcheting the floor down through the 10/25/50/100 tiers); a failure resets to
  zero and, if the category had graduated past base, sets a tier-scaled cooldown
  (15/30/60 min). A failure at base imposes no cooldown (it's already untrusted).

### 2. `ApprovalGate::evaluate_with_confidence_floor`

`evaluate()` is refactored to delegate to a new
`evaluate_with_confidence_floor(proposal, sub, now, min_confidence)` with the
configured floor — **identical existing behavior**. The trust-aware caller passes
the earned floor instead. Only the LowRisk confidence comparison uses the
parameter; every other branch (blocklist, kill switch, risk routing, blast-radius,
rate-limit + lock reservation) is unchanged.

### 3. Executor wiring (`RemediationExecutor`)

The executor optionally holds an `Arc<TrustModel>` (`with_trust`, Phase 3 opt-in).
In `process()`:

- **Decide**: with a trust model, look up `threshold_for(category, now)` and call
  `evaluate_with_confidence_floor`; in cooldown (`None`) it passes an *unreachable*
  floor (`1.01`) so the LowRisk path routes to human approval through the gate's
  normal logic — no bespoke "blocked" branch. Without a trust model, it calls
  `evaluate()` exactly as in Phase 2c.
- **Feed back**: after execution, `record(category, success, now)` — `Executed`
  (in-scope) is a success; an over-scope `RolledBack` or an ARM `Failed` is a
  failure. Outcomes are only recorded for actions the trust model authorized.

Enabled by `remediation.progressive_trust` (default `false`) on top of
`remediation.enabled`.

## Consequences

### Positive

- The platform earns autonomy: a category with a long clean track record
  auto-handles more incidents (floor down to 0.85), cutting MTTR for routine
  failures, while a fresh or recently-failed category stays conservative.
- The gate remains the single trust boundary. Trust can only *lower a LowRisk
  confidence bar within `[0.85, 0.95]`* — it cannot touch risk routing, the kill
  switch, rate limits, locks, or the HighRisk/Destructive human requirement.
- Self-correcting: one failure (including an over-scope rollback) instantly resets
  the category and cools it down, so a regression can't keep auto-firing.
- Cooldown-as-unreachable-floor means suspension reuses the gate's existing
  human-approval path — minimal new surface, nothing to special-case.
- Deterministic and offline-testable (injected clock); off by default → a pure
  superset of Phase 2c with zero behavioral change when disabled.

### Negative

- Trust state is in-memory and per-replica: it resets on restart and is not shared
  across replicas (each builds trust independently). Acceptable for a
  conservative-by-reset model; durable/shared trust (Postgres-backed) is a forward
  step. Mirrors the gate's existing in-memory rate/lock state and the dedup cache.
- The success signal here is the *executor's* immediate outcome (executed in scope
  / failed / rolled back), not yet the learning loop's post-action monitor
  (resolved-in-15-min / refired-in-1-hr). Wiring the longitudinal `OutcomeSignals`
  into trust is a follow-up; the structural seam (ADR-016 `OutcomeSignals`) exists.
- Two trust-ish objects (`ConfidenceTracker` for insight, `TrustModel` for gating).
  Intentional separation, but a reader must know which is which.

### Follow-ups

- Persist trust state (Postgres) so it survives restarts and is shared across
  replicas.
- Feed the learning loop's post-remediation `OutcomeSignals` into `record()` so
  trust reflects whether the fix *held*, not just whether the API call succeeded.
- Emit the per-category floor + consecutive successes as OTel metrics for
  operator visibility into earned autonomy.
- Slack-interactive approval for the human path (shared with ADR-009's forward step).

## References

- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — the fixed
  auto-approve policy this ADR makes adaptive; the gate, guardrails, and
  `enforced_risk()` defense it builds on without weakening.
- [ADR-016: Knowledge Store + Learning Loop](016-knowledge-store-learning-loop.md)
  — the `OutcomeSignals` / `ConfidenceTracker` feedback machinery; `TrustModel` is
  its gating-side counterpart.
- `docs/diagrams/phase3-architecture.md` — the *Progressive Trust Model* and
  *Auto-Remediation* panels this ADR implements.
