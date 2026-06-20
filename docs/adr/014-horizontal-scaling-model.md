# ADR-014: Horizontal Scaling Model (Multi-Replica)

## Status

Accepted (2026-06-20). Implements Phase 2b task #2 ("Platform runs multi-replica
behind ACA scaling"). Builds directly on ADR-013 (NATS JetStream ingest) and
ADR-012 (schema modernization — the `workflows.alert_id` UNIQUE constraint).

## Context

Phase 1 and 2a shipped a single-replica platform: one container, one
orchestrator dispatch loop, one in-process set of agents. Phase 2b's exit
criteria require the platform to run multiple replicas behind Azure Container
Apps autoscaling without double-investigating an alert, losing an alert, or
corrupting the audit trail.

The question is **not** "can we run more than one replica" — ACA will happily
schedule N copies of a stateless container. The question is **what makes
running N copies *safe*** given two ingestion paths (webhook + NATS) and a
shared Postgres store. This ADR records the invariants that make the existing
code multi-replica-safe and the one infrastructure change (an explicit scale
rule) that turns the capability on.

### What already makes replicas safe

The platform was built single-replica but with multi-replica seams in place:

| Concern | Mechanism | ADR |
|---------|-----------|-----|
| **Replicas are stateless** | No replica holds authoritative state. The orchestrator, agents, and dispatch loop are per-replica and ephemeral; the only durable state is Postgres. | ADR-001, ADR-003 |
| **Work distribution (NATS path)** | All replicas attach to the **same durable pull consumer** (`sre-orchestrator`). JetStream distributes messages across the attached subscribers — each message is delivered to exactly one replica at a time, redelivered elsewhere only on ack-timeout. | ADR-013 |
| **Cross-replica dedup** | `workflows.alert_id` carries a UNIQUE constraint. `try_create_workflow` returns `false` (not an error) when another replica already claimed an alert. This is the **authoritative** gate. | ADR-012, ADR-013 |
| **Intra-replica dedup fast path** | A bounded in-memory LRU per replica absorbs the high-frequency duplicate case without a DB round-trip. It is intentionally per-replica — the DB constraint is the cross-replica authority, the LRU is only an optimization. | ADR-013 |
| **Graceful drain** | On SIGTERM each replica trips its cancellation token, stops pulling from NATS, and drains in-flight investigations within a 25s budget (inside ACA's 30s SIGTERM→SIGKILL grace window). | ADR-007, ADR-013 |
| **Readiness gating** | `/readyz` reports unhealthy while the channel/semaphore is saturated, so ACA's load balancer stops routing new webhook traffic to a saturated replica before it 503s. | ADR-007 |

The duplicate-delivery matrix the design must survive:

| Scenario | Outcome |
|----------|---------|
| Same alert arrives at two replicas via webhook (Action Group retry hit a different replica) | Both call `try_create_workflow`; one wins (Ok(true)), the other gets Ok(false) → `IngestDeduped` audit event, no second investigation. |
| Same alert arrives via webhook on replica A and NATS on replica B | Same as above — DB UNIQUE constraint arbitrates regardless of path or replica. |
| NATS redelivers an un-acked message to a different replica after `ack_wait` | The original replica either finished (DB row exists → dedup) or died (its work was never persisted → the redelivery legitimately re-runs it). No loss. |
| Replica receives SIGTERM mid-investigation | Drains within 25s; the in-flight investigation completes and persists, or NATS redelivers it to a surviving replica after `ack_wait`. |

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Stateless replicas + shared durable NATS consumer + DB-authoritative dedup** (chosen) | No new components. Work distribution is JetStream's job; dedup is the DB's job; both already exist. Scaling is a config flag. | Relies on the durable-consumer + UNIQUE-constraint invariants being respected by all future ingestion paths — enforced by routing every path through the `mpsc::Sender<AlertInfo>` seam (ADR-013). |
| B. Leader election (one active orchestrator, others standby) | Conceptually simple "one brain". | Wastes N-1 replicas; reintroduces a single point of failure; needs a lease mechanism (etcd/Postgres advisory lock) the platform doesn't have. Defeats the point of horizontal scale. |
| C. Partition alerts by resource hash across replicas | Deterministic ownership, no dedup needed. | Requires a coordinator to assign partitions and rebalance on scale events; brittle under replica churn. Over-engineered when at-least-once + idempotent dedup already works. |

## Decision

Adopt **Option A**. The platform scales horizontally as **stateless replicas**;
correctness comes from two invariants already in the codebase:

1. **At-least-once delivery + idempotent dispatch.** Every ingestion path
   terminates at `mpsc::Sender<AlertInfo>`; `Orchestrator::investigate()`
   dedups on `alert_id` (in-memory LRU → DB UNIQUE constraint). Re-running a
   never-persisted investigation is safe; re-running a persisted one is
   suppressed.

2. **No shared mutable in-process state across replicas.** All durable state is
   Postgres. Replicas may be added or removed at any time.

### Infrastructure change

The ACA app already declares `min_replicas = 1, max_replicas = 3`. Without an
explicit scale rule, ACA falls back to an implicit default; this ADR makes the
trigger explicit and tunable:

- **HTTP scale rule** (`concurrent_requests = 20`): scales the webhook ingest
  path on concurrent request load. `max_replicas` raised to **5** for headroom.
- **min_replicas** stays at **1** in staging (cost). **Production should set
  `min_replicas = 2`** so a single replica failure never drops ingestion — noted
  as a deploy-time override, not baked into the staging module.

### Forward step: NATS-driven autoscaling

When NATS is provisioned in staging (`nats.enabled = true`), the webhook HTTP
scale rule should be complemented by a **KEDA NATS JetStream scaler**
(`custom_scale_rule`) keyed on the consumer's pending count
(`nats_consumer_pending`, already exported as an OTel metric by ADR-013 PR-2).
That ties replica count to ingestion backlog rather than HTTP concurrency. It is
deliberately out of scope here because there is no NATS server in staging yet; a
scale rule pointing at a non-existent broker would never fire.

## Consequences

### Positive

- Horizontal scale is now a configuration concern, not an architectural one.
- No new failure modes: the dedup + drain invariants that protected the
  single-replica platform are exactly the ones that make N replicas safe.
- The OTel metrics from ADR-013 PR-2 (`nats_consumer_pending`,
  `nats_redeliveries_total`) are the signals an operator (and a future KEDA
  scaler) uses to reason about scale.

### Negative

- The in-memory dedup LRU is per-replica, so the **same** alert can pass two
  replicas' LRUs and reach the DB twice before one is rejected — by design; the
  cost is one extra `INSERT ... ON CONFLICT` no-op, not a double investigation.
- HTTP-concurrency scaling is a proxy for load; it can over- or under-scale
  relative to actual investigation cost. The NATS-pending scaler (forward step)
  is the more precise signal once NATS is deployed.
- `min_replicas = 1` in staging means a brief ingestion gap during a replica
  restart; acceptable for staging, which is why prod is told to use 2.

### Follow-ups

- Provision NATS in staging, then add the KEDA NATS JetStream `custom_scale_rule`.
- Set `min_replicas = 2` in the production module when it lands.
- Load-test re-validation (Phase 2b exit criterion) against a multi-replica
  deployment once NATS-driven scaling is in place.

## References

- [ADR-007: CI/CD, Containerization, ACA Deployment](007-cicd-containerization-aca-deployment.md) — drain + probes.
- [ADR-012: Schema Type Modernization](012-schema-type-modernization.md) — the `alert_id` UNIQUE constraint this model relies on.
- [ADR-013: NATS JetStream as Parallel Ingestion Path](013-nats-jetstream-ingest.md) — durable consumer work distribution + layered dedup.
