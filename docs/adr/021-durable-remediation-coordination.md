# ADR-021: Durable Remediation Coordination (Cross-Replica Exactly-Once + Crash Recovery)

## Status

Proposed (2026-06-20). Drafted in response to the 2026-06-20 distributed-architecture
review. This ADR **completes the deferred parts of ADR-009** (§7 lifecycle states that
were never migrated) and **narrows the scope claimed by ADR-014** (horizontal scaling):
ADR-014's safety argument is correct for the *read/investigate* path and silent on the
*write/remediate* path. Until this ADR is implemented, enabling
`remediation.enabled = true` with `max_replicas > 1` is unsafe and must be blocked
(see §Decision part 7).

## Context

Two prior ADRs set up a gap neither of them closes:

- **ADR-009 (Remediation Safety Model)** defines the safety envelope: a per-subscription
  auto-approve rate limit, a per-resource mutual-exclusion lock, and a blast-radius cap.
  These are *invariants*, not suggestions — threats #2 ("runaway remediation cascade")
  and #8 ("race between auto-execution and manual operator intervention") are mitigated
  **only** by them.
- **ADR-014 (Horizontal Scaling Model)** declares the platform multi-replica-safe. Its
  argument rests on one cross-replica authority: the `workflows.alert_id` UNIQUE
  constraint (`crates/store/migrations/20260518000001_workflow_alert_id_unique.sql:41`),
  enforced via `INSERT … ON CONFLICT (alert_id) DO NOTHING`
  (`crates/store/src/postgres.rs:87-109`).

The problem: **that constraint gates the creation of an *investigation*, not the
execution of a *write action*.** Every remediation coordination primitive ADR-009
specifies is implemented in *process memory*, shared only *within* a single replica:

- The approval gate's state is `Mutex<GateState>` holding the rate-limit window, the
  per-resource locks (`HashSet<String>`), and the pending-approval tally
  (`crates/gates/src/gate.rs:12-40`), shared as `Arc<ApprovalGate>` — process-local by
  construction (`gate.rs:24-28`).
- The rate limiter is an in-process `HashMap<String, VecDeque<DateTime>>`
  (`crates/gates/src/guardrails.rs:39-71`).
- Progressive-trust state (ADR-017) is likewise per-replica, as ADR-017 §Consequences
  already admits (lines 111-114).

The deployed topology is N stateless replicas on ACA
(`revision_mode = "Single"`, `min_replicas = 1`, `max_replicas = 5` —
`infra/staging/main.tf:65,100-101`) with **no NATS provisioned**
(`config/default.toml` nats section commented out; `infra/staging/main.tf:105-109`
describes NATS only as "the forward step"). So work is fanned out by the ACA HTTP load
balancer, and nothing reconciles state across replicas except the single `alert_id`
constraint.

### Why this is latent today — and why that is the reason to act now

Remediation is disabled by default and **ADR-011 (identity separation) has not landed**
— the deployed app runs on a single broad `sre-agents-identity`
(`infra/staging/main.tf:79-82`), and ADR-011 §Consequences blocks remediation until a
scoped `id-sre-agents-remediation` identity exists. So none of the gaps below are live
in production *yet*. That window — remediation written but not switched on — is exactly
when the coordination layer should be made durable, before a future "enable remediation"
PR inherits three latent distributed-systems bugs.

### The three gaps this ADR closes

**G1 — No action-level idempotency around ARM writes.**
`remediate()` calls `pipeline.executor.process(proposal, now)` and issues the ARM
mutation with no dedup of any kind (`crates/orchestrator/src/engine.rs:227-261`); each
proposal is tagged with a fresh `Uuid::new_v4()`
(`crates/agents/src/remediation.rs:103`), so the identifier is non-deterministic and
useless as an idempotency key. The `alert_id` constraint makes *re-investigation* safe,
but it does **not** make *re-execution* safe. This matters the moment any path re-drives
a workflow that already executed a write:

- the crash-recovery reconciler this ADR introduces (G3),
- a NATS redelivery that re-runs an investigation whose first attempt never persisted
  (ADR-014 line 43 explicitly relies on this re-run),
- any future retry around the ARM call.

Without a deterministic, durable action key, every one of those re-executes the
mutation. **You cannot safely build crash recovery (G3) without idempotency (G1)** — the
reconciler would re-fire half-completed remediations. They must ship together.

**G2 — Cross-replica guardrail evaporation.**
Because the rate limiter and per-resource locks live in per-process memory, the simplest
trigger is mundane and common: **two *different* alerts about the *same* Azure resource**
(a resource routinely has several alert rules — CPU, memory, restarts). They carry
distinct `alert_id`s, so both pass the `alert_id` constraint, create workflows on two
different replicas, and both reach `remediate()` for an action touching that one
resource. The per-resource mutual-exclusion lock (`gate.rs` `locks: HashSet`) is local
to each replica, so neither sees the other → **concurrent mutation of the same
resource**, the exact race ADR-009 threat #8 promises to prevent. Likewise the
"5 auto-approvals/hour/subscription" cap (ADR-009 §4) becomes effectively `5 × replicas`
per hour — ADR-009's runaway-cascade defense (threat #2) is `max_replicas`× too
permissive.

**G3 — No durable remediation lifecycle or crash recovery.**
Live investigation state is the in-memory `InvestigationWorkflow`
(`crates/orchestrator/src/workflow.rs:14-24`); Postgres writes are best-effort and
log-and-continue on failure (`engine.rs:407-413`). The `workflow_state` ENUM never
gained the `Proposing`/`AwaitingApproval`/`Executing`/`RolledBack` values ADR-009 §7
promised (confirmed in `crates/store/migrations/20260420000001_fix_schema_types.sql:13-21`),
so there is no durable "a write is in flight" marker. `get_active_workflows()` exists
(`crates/store/src/postgres.rs:173`) but nothing calls it at startup. A replica killed
between "ARM returned 200" and "audit event appended" — routine during ACA revision
swaps — leaves an **executed mutation with no record**, and a workflow stranded in
`investigating` forever.

### Out of scope

- **Identity separation (ADR-011)** remains a hard prerequisite for enabling remediation;
  this ADR assumes it lands first and does not re-litigate it.
- **Moving the agent / reply-channel topology off in-process `mpsc`** (the ADR-006 /
  ADR-013 distributed-agent question). Remediation correctness does not require it — a
  single replica's investigation is internally consistent; the gaps here are purely
  about *cross-replica* write coordination and *crash* recovery.
- **Knowledge-store / RAG availability** (a separate review follow-up).
- **Azure-side API rate limiting** (the read-path `governor` work) — tracked separately.

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Postgres-backed coordination** (chosen) | No new infrastructure — Postgres is *already* the cross-replica authority for dedup (ADR-014), so the trust model is unchanged. Idempotency reuses the exact `INSERT … ON CONFLICT DO NOTHING` claim pattern already proven in `try_create_workflow`. Locks, rate budgets, and trust counters become durable and survive replica churn. One transaction makes reserve-claim-lock atomic across replicas. | Adds DB round-trips on the (low-frequency) write path. Lock leases need expiry to survive a dead holder. ENUM `ADD VALUE` has a migration-transaction caveat (handled in §Decision part 5). |
| B. Redis / dedicated distributed-lock manager (Redlock) | Fast; purpose-built for locks and rate windows; TTLs are native. | A *new* stateful dependency to provision, secure, and operate — and to reason about during partitions (Redlock correctness is contested). Remediation is rare; sub-millisecond locking is not the constraint. Splits the source of truth across two stores. |
| C. Leader election — one replica executes all remediations | Conceptually a single brain; no cross-replica race by construction. | Reintroduces a single point of failure and wastes N-1 replicas — the exact anti-pattern ADR-014 Option B rejected. Needs a lease mechanism anyway (Postgres advisory lock), at which point Option A's per-resource locks are strictly more granular and cheaper. |
| D. Keep in-memory; pin `max_replicas = 1` whenever remediation is enabled | Zero code. | A foot-gun: one Terraform edit silently removes every remediation safety invariant. Caps remediation throughput and availability to a single replica. Acceptable only as the *interim* guard (part 7), not the destination. |

## Decision

Adopt **Option A**: make remediation coordination durable in Postgres, reusing the
claim-by-unique-constraint pattern that already arbitrates investigations. Seven parts.

### 1. Deterministic action idempotency key

The proposal's random `Uuid` is kept as a display/correlation id, but **claiming and
deduping key off a deterministic hash of the action's semantic content**, so the same
action computed by any replica (or any re-drive) produces the same key:

```
action_key = sha256(
    investigation_id
    || category
    || sort(scope_resources)            // canonical order — set, not list
    || canonical_json(arm_request)      // method + path + sorted body
)
```

SHA-256 (the `sha2` crate is already a dependency) over a canonical serialization.
`scope_resources` is sorted so resource ordering can't change the key.

### 2. `remediation_actions` ledger — claim + audit + rate-limit source of truth

```sql
CREATE TABLE remediation_actions (
    action_key        TEXT PRIMARY KEY,          -- deterministic hash (part 1)
    proposal_id       UUID NOT NULL,
    investigation_id  UUID NOT NULL REFERENCES workflows(id),
    subscription_id   TEXT NOT NULL,
    resource_type     TEXT NOT NULL,
    category          TEXT NOT NULL,
    risk              TEXT NOT NULL,
    state             remediation_action_state NOT NULL,  -- claimed→executing→executed|failed|rolledback
    claimed_by        TEXT NOT NULL,             -- replica id (service.instance.id)
    claimed_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    executed_at       TIMESTAMPTZ,
    arm_response      JSONB,
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX remediation_actions_sub_time ON remediation_actions (subscription_id, claimed_at);
CREATE INDEX remediation_actions_rtype_time ON remediation_actions (resource_type, claimed_at);
```

**Claim** (only the winner executes — same pattern as `try_create_workflow`):

```sql
INSERT INTO remediation_actions (action_key, proposal_id, investigation_id,
    subscription_id, resource_type, category, risk, state, claimed_by)
VALUES ($1, …, 'claimed', $replica)
ON CONFLICT (action_key) DO NOTHING
RETURNING action_key;          -- row returned ⇒ we own it; no row ⇒ already claimed, skip execution
```

**Rate limit** is now a query against the ledger, not an in-memory window — making it the
authoritative cross-replica count:

```sql
SELECT count(*) FROM remediation_actions
WHERE subscription_id = $1 AND claimed_at > now() - interval '1 hour';   -- vs ADR-009 §4 cap of 5
```

### 3. `remediation_locks` table — cross-replica per-resource mutual exclusion

```sql
CREATE TABLE remediation_locks (
    resource_id    TEXT PRIMARY KEY,
    proposal_id    UUID NOT NULL,
    investigation_id UUID NOT NULL,
    holder         TEXT NOT NULL,                -- replica id
    acquired_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at     TIMESTAMPTZ NOT NULL          -- lease; auto-frees a dead holder's lock
);
```

**Acquire** (steals only an *expired* lease; otherwise acquisition fails):

```sql
INSERT INTO remediation_locks (resource_id, proposal_id, investigation_id, holder, expires_at)
VALUES ($1, $2, $3, $replica, now() + $lease)
ON CONFLICT (resource_id) DO UPDATE
    SET proposal_id = EXCLUDED.proposal_id, investigation_id = EXCLUDED.investigation_id,
        holder = EXCLUDED.holder, acquired_at = now(), expires_at = EXCLUDED.expires_at
    WHERE remediation_locks.expires_at < now()   -- only steal a dead lease
RETURNING resource_id;                            -- row returned ⇒ acquired; no row ⇒ held by a live remediation
```

Lease defaults to **5 minutes** (longer than any single remediation execution). Released
explicitly with `DELETE` on completion; expiry is the backstop for a crashed holder.

### 4. Atomic reserve-claim-lock

The in-memory critical section that today reserves rate + lock without crossing an await
(`crates/gates/src/gate.rs:153-187`) becomes a single Postgres transaction so the
check-then-act is atomic across replicas:

```rust
let mut tx = pool.begin().await?;
// Serialize the rate check + claim per subscription to close the TOCTOU window where
// two near-limit claimers both pass the count check. Auto-released at COMMIT/ROLLBACK.
sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))").bind(&subscription_id).execute(&mut *tx).await?;

if rate_count(&mut tx, &subscription_id).await? >= cfg.sub_rate { tx.rollback().await?; return Reserve::RateLimited; }
for r in &scope_resources {
    if !acquire_lock(&mut tx, r, …).await? { tx.rollback().await?; return Reserve::ResourceBusy; }
}
if !claim_action(&mut tx, &action_key, …).await? { tx.rollback().await?; return Reserve::AlreadyDone; }
tx.commit().await?;   // we exclusively own the locks + the action claim
```

The `ApprovalGate` keeps its entire policy brain — `enforced_risk()` re-derivation,
dual-distinct-approver, blocklist-first, `RollbackStrategy::None ⇒ Destructive`
(all sound, see review §"What's good"). Only its *state backend* moves from `Mutex<GateState>`
to this transaction. The gate's public API is unchanged; `GateState` is swapped for a
`PgGateStore`.

### 5. Complete ADR-009 §7 — durable lifecycle + write-intent-before-ARM

Add the lifecycle states ADR-009 §7 specified but never migrated. **Caveat:** Postgres
`ALTER TYPE … ADD VALUE` cannot be used in the same transaction that then uses the value,
and `sqlx::migrate!` wraps each migration in a transaction — so the ENUM additions go in
their **own** migration file, ahead of any migration or code that references the new
values.

```sql
-- migration N: extend workflow_state (must be its own migration)
ALTER TYPE workflow_state ADD VALUE IF NOT EXISTS 'proposing';
ALTER TYPE workflow_state ADD VALUE IF NOT EXISTS 'awaitingapproval';
ALTER TYPE workflow_state ADD VALUE IF NOT EXISTS 'executing';
ALTER TYPE workflow_state ADD VALUE IF NOT EXISTS 'rolledback';

-- migration N+1: remediation_action_state ENUM + the tables in parts 2 & 3
CREATE TYPE remediation_action_state AS ENUM ('claimed','executing','executed','failed','rolledback');
```

The executor writes the `executing` row **before** the ARM call (intent log) and the
`executed`/`failed` row after, both inside the owning transaction's follow-up. An
executed-but-uncommitted mutation is now detectable: a `remediation_actions` row stuck in
`executing` past its lock lease is the recovery signal.

### 6. Startup reconciler

On boot each replica scans for work a dead replica abandoned and reconciles it
**idempotently** (this is why part 1 is a prerequisite):

- `remediation_actions` in `executing` whose lock lease has expired → query ARM for the
  resource's current state; mark `executed` (action took effect) or `failed`; never blind-retry.
- `workflows` in non-terminal states (`get_active_workflows()`, already present at
  `postgres.rs:173`) with no live owner → fail-and-escalate, or re-drive. Re-drive is safe
  *only* because the action key (part 1) makes re-execution a no-op claim.

### 7. Interim guard until parts 1–6 land

Add a startup assertion: if `remediation.enabled == true` **and** the effective
`max_replicas > 1`, refuse to start (or log a `CRITICAL` and force single-flight) until
this ADR is implemented. This makes Option D's foot-gun explicit and self-documenting
rather than a silent loss of safety. Move progressive-trust counters (ADR-017) into a
small `remediation_trust` table in the same migration so trust decisions stop diverging
per replica.

## Consequences

### Positive

- **Exactly-once-effect remediation across replicas.** Two alerts about one resource, a
  redelivery, or a recovery re-drive all converge on one claim — at most one ARM
  execution. Closes G1 and the core of G2.
- **ADR-009's safety envelope finally holds at scale.** Rate limits and per-resource
  locks are cross-replica; threats #2 and #8 are mitigated in the *deployed* topology,
  not just single-replica.
- **Crash recovery becomes possible and safe.** Durable lifecycle states + the reconciler
  turn "executed but unrecorded" from a silent corruption into a detectable, reconcilable
  condition. Closes G3.
- **No new infrastructure or trust boundary.** Postgres was already the cross-replica
  authority; this extends the pattern that already works.
- **ADR-017 trust stops diverging** once counters are shared (part 7).

### Negative

- **Write-path latency** gains a few DB round-trips (one transaction per proposal).
  Negligible: remediation is rare and human-gated for everything but `LowRisk`.
- **Lock-lease tuning.** Too short → a slow ARM call loses its lock mid-execution; too
  long → a crashed holder blocks a resource until expiry. 5 min is a starting point;
  revisit after load testing.
- **Migration ordering discipline.** The ENUM-then-use split (part 5) is a known Postgres
  gotcha; getting it wrong fails the migration. Documented inline.
- **Reconciler correctness depends on ARM being queryable** to determine whether an
  `executing` action took effect. If ARM is unreachable at boot, those actions stay
  `executing` until ARM recovers — acceptable (fail-safe: do nothing rather than
  re-execute blindly).

### Follow-ups

- Implement as a stacked PR series mirroring the parts: (PR-1) migrations + `PgGateStore`
  behind the unchanged `ApprovalGate` API; (PR-2) executor write-intent + reconciler;
  (PR-3) trust-state table + the part-7 startup guard.
- Amend **ADR-014** to state plainly that its multi-replica-safety claim covers the
  investigate path; the remediate path's authority is this ADR.
- Update **ADR-009 §7** status to "implemented by ADR-021" once PR-1 lands.
- Correct `docs/PROGRESS.md` "Known Issues" (the in-memory-only remediation guardrails
  belong on that list until this ships).
- Load-test a multi-replica deployment with remediation enabled and two alerts targeting
  one resource — the regression test for G2.

## References

- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — the envelope this ADR makes durable; completes its deferred §7 lifecycle states.
- [ADR-011: Identity Separation for Remediation](011-identity-separation-remediation.md) — hard prerequisite; remediation stays disabled until it lands.
- [ADR-012: Schema Type Modernization](012-schema-type-modernization.md) — the `workflow_state` ENUM extended here.
- [ADR-013: NATS JetStream Ingest](013-nats-jetstream-ingest.md) — at-least-once delivery and the redelivery re-run this ADR's idempotency makes safe.
- [ADR-014: Horizontal Scaling Model](014-horizontal-scaling-model.md) — scope corrected: investigate-path safety only.
- [ADR-017: Progressive-Trust Auto-Remediation](017-progressive-trust-auto-remediation.md) — per-replica trust state moved durable in part 7.
- The `try_create_workflow` claim pattern (`crates/store/src/postgres.rs:87-109`) — the proven precedent this ADR generalizes to the write path.
