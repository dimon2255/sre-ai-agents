# ADR-003: Event-Driven Agent Communication via Structured Message Envelopes

## Status

Accepted (2026-04-05)

## Context

Agents in the SRE platform need to exchange information: the orchestrator sends investigation tasks, agents return findings, agents may request escalation. We need a communication model that supports:

- Parallel execution (multiple agents investigating simultaneously)
- Traceability (every message linked to an investigation via correlation ID)
- Type safety (invalid payloads caught at compile time, not runtime)
- Auditability (full message history for post-incident review)
- Extensibility (new message types as agents are added)

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Structured message envelopes via typed channels** | Type-safe, traceable, auditable | Shared types create coupling between crates |
| Unstructured JSON blobs | Maximum flexibility, no shared types | Runtime errors from malformed payloads, no compile-time guarantees |
| Shared mutable state (Arc<Mutex<>>) | Simple for small systems | Contention at scale, hard to audit, no message history |
| Full actor framework (Actix) | Battle-tested actor model | Heavy dependency, more machinery than needed |

## Decision

**Structured message envelopes** with Rust enums, transported over **Tokio mpsc channels** in Phase 1 and **NATS JetStream** in Phase 2.

### Message Envelope

```rust
pub struct AgentMessage {
    pub id: Uuid,                          // Unique message ID
    pub correlation_id: Uuid,              // Ties all messages in one investigation
    pub source_agent: AgentId,             // Who sent this
    pub target: MessageTarget,             // Who receives this
    pub payload: AgentPayload,             // Typed payload (enum)
    pub confidence: f64,                   // 0.0-1.0, drives escalation decisions
    pub timestamp: DateTime<Utc>,          // When created
    pub parent_message_id: Option<Uuid>,   // DAG of reasoning chain
}
```

### Payload Types

```rust
pub enum AgentPayload {
    InvestigationRequest(InvestigationContext),
    Finding(Finding),
    RemediationProposal(Remediation),
    EscalationNotice(Escalation),
    ToolResult(ToolOutput),
    HealthCheck(AgentHealth),
}
```

### Transport

| Phase | Transport | Why |
|-------|-----------|-----|
| Phase 1 | `tokio::sync::mpsc` channels | In-process, zero overhead, sufficient for single instance |
| Phase 2 | NATS JetStream | Persistent messaging, replay capability, horizontal scaling |

### Agent Handle

Each agent is a long-lived async task. The orchestrator holds a handle:

```rust
pub struct AgentHandle {
    pub id: AgentId,
    pub sender: mpsc::Sender<AgentMessage>,
    pub agent_type: AgentType,
    pub status: watch::Receiver<AgentStatus>,
}
```

### Message Persistence

Every message is appended to the event log (PostgreSQL; was SQLite in Phase 1) before delivery. This gives us:

- Full audit trail for any investigation
- Replay capability for debugging and testing
- Post-incident review of agent decision chains

## Consequences

### What becomes easier

- **Type safety** — Rust enums make invalid message payloads a compile error, not a runtime surprise.
- **Correlation** — `correlation_id` on every message gives distributed tracing across the agent graph for free.
- **Auditability** — Append-only message log answers "why did the system do X?" in seconds.
- **Testing** — Typed messages make it straightforward to assert on agent behavior in unit tests.

### What becomes harder

- **Cross-crate coupling** — All agents depend on `sre-common` for message types. Adding a new payload variant requires recompiling all crates. Acceptable trade-off for type safety.
- **Schema evolution** — Changing `AgentPayload` variants is a breaking change. Mitigation: use `#[non_exhaustive]` on enums, add new variants additively.
- **Transport migration** — Moving from mpsc to NATS in Phase 2 requires serialization (messages must become `Serialize + Deserialize`). Plan: implement serde traits from day 1 even though Phase 1 doesn't serialize.
