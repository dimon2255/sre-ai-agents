# ADR-013: NATS JetStream as Parallel Ingestion Path

## Status

Proposed (2026-05-18). First implementation in `feat/nats-jetstream-ingest`.

## Context

Phase 1 and 2a delivered a single ingestion path: an Azure Action Group POSTs the Common Alert Schema to `/api/v1/alerts/azure`, the handler parses it into an `AlertInfo`, and sends it on `mpsc::Sender<AlertInfo>` to the orchestrator dispatch loop. The handler is HMAC-authenticated (ADR-008) and returns 503 when the channel is full so Action Groups retry with backoff.

The webhook is adequate for Azure-originated alerts but has structural limits:

| Limit | Impact |
|-------|--------|
| **Point-to-point, synchronous** | Only Action Groups can deliver. A second producer (synthetic monitor, internal pipeline, replay harness) has to either re-implement the HMAC client or be plumbed differently. |
| **No replay** | If the platform is down or the channel is full and Action Groups give up, the alert is lost. There is no durable buffer behind the HTTP edge. |
| **No fan-out** | One subscriber by design. A second consumer (audit pipeline, parallel test environment) cannot tap the same alert stream without changes upstream. |
| **Backpressure is per-request** | The 503-and-retry pattern works but couples the platform's liveness to the Action Group retry policy. A short outage of the orchestrator loop is recoverable only on Azure's schedule. |

Phase 2b plans LLM-assisted reasoning, an Agent SDK for remediation, and additional producers. A durable, replayable, decoupled ingestion bus is the prerequisite — not a "nice to have."

The platform already uses `tokio::sync::mpsc` channels internally for agent communication (ADR-003, ADR-006). The architectural question is **where the boundary between external transport and internal communication lives**, not whether to replace the internal channels.

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Parallel ingestors, shared sender** — Add a `NatsIngestor` task alongside the axum webhook. Both hold an `mpsc::Sender<AlertInfo>`. Orchestrator and agents are untouched. | Smallest blast radius. Webhook continues to work for direct Azure delivery. NATS adds durability, replay, and a path for non-Azure producers. Decision is reversible — disabling the NATS task removes it without touching anything else. | Two ingestion paths require dedup. NATS-down must not cascade into platform-down (operational discipline, not architectural risk). |
| B. Replace the webhook with NATS-only ingestion | Single ingestion contract; simpler mental model long-term. | Cutover risk against a working production path. Breaks the ADR-008 HMAC story. Adds an Azure→NATS bridge as a new prod component (Logic App, Function, or sidecar) before the platform itself sees any benefit. |
| C. Introduce an `Ingestor` trait abstracting webhook and NATS behind a common interface | Slight uniformity gain for shutdown/lifecycle. | Premature abstraction for two implementations whose shapes diverge (request/response vs. pull/ack). Adds indirection without justifying complexity. |

## Decision

**Option A.** NATS JetStream is added as a parallel ingestion path. The webhook is unchanged in behavior. Both paths terminate at the same `mpsc::Sender<AlertInfo>`. Agents and orchestrator are untouched.

### Boundary Invariant

The architectural seam for this ADR is `mpsc::Sender<AlertInfo>` in `webhook::AppState` (`src/webhook.rs`) and the matching `Receiver` in the orchestrator dispatch loop (`src/main.rs::run_server`). Everything above the seam is **ingestion** (boundary). Everything below is **investigation** (internal). NATS-related code stays above the seam exclusively. No file under `crates/orchestrator`, `crates/agents`, or `crates/store` may import `async-nats`.

### Subject and Stream Design

- **Stream name:** `sre-alerts`
- **Storage:** file (durable across restarts)
- **Retention:** limits-based, `max_age = 24h`, `max_msg_size = 64 KB` (matches the existing `DefaultBodyLimit` on the webhook)
- **Replicas:** 1 in staging, 3 in production (configured per-environment)
- **Subjects:** `alerts.azure.>` for Azure Common Alert Schema payloads; `alerts.dlq.azure.>` reserved for the future DLQ stream (PR-3, not this PR)

### Consumer Design

- **Name:** `sre-orchestrator` (durable)
- **Type:** pull consumer (explicit batch fetch, no server-driven push)
- **Ack policy:** explicit
- **Ack wait:** 60 seconds — must be longer than the longest blocking path between `fetch()` and `ack()`
- **Max deliver:** 5 — terminal failure after 5 redelivery attempts; will route to DLQ once PR-3 lands
- **Max ack pending:** matches `webhook.channel_capacity` (default 64) — JetStream stops delivering when the in-process pipeline is saturated, which is the entire backpressure story

### Backpressure Contract

The ingestor MUST NOT acknowledge a JetStream message until the message has been accepted by the `alert_tx` channel. The sequence is:

1. `fetch()` returns a batch
2. For each message: parse → `alert_tx.try_send(alert_info)`
3. On `Ok(())` → `msg.ack().await`
4. On `Err(TrySendError::Full)` → do not ack; let `ack_wait` expire and JetStream redeliver
5. On parse failure → `msg.term().await` (poison; do not redeliver; PR-3 routes these to DLQ)

This makes JetStream the durable buffer behind the in-process channel. Backpressure from the orchestrator surfaces as redelivery latency, not message loss.

### Idempotency (Dedup)

JetStream provides at-least-once delivery, and Option A introduces the possibility of the same alert arriving via both the webhook and NATS during cutover or replay. The `Orchestrator::investigate()` entry point gains a bounded in-memory LRU keyed by `alert_id`:

- Capacity: `webhook.channel_capacity * 10` (default 640)
- On hit: log "duplicate alert skipped" and return `Ok` without dispatching
- On miss: insert and proceed

The LRU is intentionally in-memory only. The durable workflow store (`workflows.alert_id`) is the authoritative dedup record, but a DB roundtrip per delivery is unjustified cost. The LRU absorbs the high-frequency case; the DB will catch a duplicate that survives the LRU eviction window.

### NATS Authentication

For PR-1 the connection uses an unauthenticated dev configuration (local NATS for tests, optional in staging). Production wiring uses NATS NKEY/JWT via a credentials file pointed at by `nats.credentials_path`. Credentials live in the same Azure Key Vault secret store path as `SRE__STORE__DATABASE_URL` and other secrets. **No NATS credentials are committed; no fallback to anonymous in staging or production.**

### Failure Isolation

- `healthz` (liveness) does NOT check NATS. A JetStream blip must not kill the container.
- `readyz` MAY check NATS but only flag failure if **both** the webhook channel and the NATS consumer are unhealthy. The webhook is the safety net.
- The NATS task is spawned with restart-on-disconnect via `async-nats` reconnect logic. Connection loss is logged at WARN; sustained outage (>5 min) escalates to ERROR via tracing.

### Graceful Shutdown

Both the axum server and the NATS ingestor task receive a shared `tokio_util::sync::CancellationToken`. On Ctrl+C / SIGTERM:

1. The cancellation token is tripped
2. axum's `with_graceful_shutdown` future completes
3. The NATS ingestor's `select!` arm matches the token and exits the fetch loop after acking any messages it had already enqueued
4. The orchestrator drain phase (existing in `main.rs`) proceeds as today

### Out of Scope for PR-1

- DLQ stream and consumer (`alerts.dlq.azure.>`) — PR-3
- OTel metrics for `nats_consumer_pending`, `nats_redeliveries_total`, `nats_dlq_total` — PR-2
- Azure → NATS bridge (Logic App / Function) — out of scope entirely; this ADR governs the consumer side only
- Reply-subject refactor of `InboundMessage` (memory note: `InboundMessage.reply_tx` is a `mpsc::Sender`, non-serializable). Not needed until agent replies cross NATS, which is a Phase 3 concern.

## Consequences

### What becomes easier

- **A second producer can be added without touching the platform.** Anyone with publish permission on `alerts.azure.>` can feed investigations: synthetic monitors, replay harnesses, partner systems.
- **Replay becomes a one-liner.** `nats consumer create --replay-policy=original sre-alerts replay-once` against the existing durable stream replays an arbitrary historical window for debugging or post-mortem analysis.
- **Decouples platform liveness from upstream retry policies.** A 30-second orchestrator pause becomes a 30-second consumer pending count, not 30 seconds of dropped Action Group retries.
- **The boundary between ingestion and investigation is now explicit and documented.** Future ingestion changes (gRPC API, Slack-triggered manual investigations, scheduled scans) plug into the same `mpsc::Sender<AlertInfo>` seam.

### What becomes harder

- **Local dev and CI gain a NATS dependency.** `testcontainers-modules` covers the test side; local `cargo run -- serve` requires either `nats.enabled = false` (default) or a running NATS server.
- **Two ingestion paths increase the surface area for operators to reason about.** Mitigated by the in-memory dedup LRU and the explicit failure-isolation rule.
- **Credential management adds NATS NKEY/JWT to the secrets footprint.** Mitigated by colocating with existing Key Vault paths.

### Rollback Plan

The NATS ingestor task is gated by `nats.enabled` in config. Rollback is `SRE__NATS__ENABLED=false` followed by a restart — no schema migration, no data loss, no agent or orchestrator code path is changed. The webhook continues to receive alerts as before.

### Reversibility

The Option A boundary makes this ADR a low-cost reversal target. If JetStream proves operationally heavier than its value (unlikely; NATS is purpose-built for this), the rollback is the single config flag above plus removal of the `async-nats` dependency.
