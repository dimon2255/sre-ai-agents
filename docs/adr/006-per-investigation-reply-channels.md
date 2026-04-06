# ADR-006: Per-Investigation Reply Channels

## Status

Accepted (2026-04-06)

## Context

The original orchestrator design (ADR-003) used a single shared `mpsc::Receiver` as the orchestrator's inbox. All agents held a clone of the corresponding `mpsc::Sender` and sent all responses — triage results, analysis findings, health checks — to this one channel.

### The Race Condition

When the orchestrator dispatched a task to the TriageAgent and then called `recv()` on the shared inbox, it assumed the next message would be the triage response. This held true in Phase 1's single-investigation mode, but would fail under concurrent investigations or if any agent sent an unexpected message (health status, late response from a prior investigation). Messages from different investigations would be interleaved on the shared inbox, causing:

- **Misattributed findings**: An analysis result from Investigation A consumed by Investigation B
- **Silent data corruption**: Reports containing findings from the wrong investigation
- **Incorrect escalation decisions**: Confidence scores calculated from mixed findings

This was identified during code review as a blocker for Phase 2's concurrent investigation support.

### Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **Per-investigation reply channels** | Clean isolation, zero filtering, simple reasoning | Changes SreAgent trait signature, agents must accept reply_tx per message |
| Shared inbox + correlation-ID filter | Minimal API change | Requires stash/reprocessing logic, effectively a custom message broker |
| Per-investigation inbox (separate receiver per investigation) | Full isolation | Requires agents to hold multiple senders, more complex lifecycle |

## Decision

**Per-investigation reply channels bundled with each task message.**

### Design

A new `InboundMessage` type wraps the existing `AgentMessage` with a per-investigation reply channel:

```rust
pub struct InboundMessage {
    pub message: AgentMessage,
    pub reply_tx: mpsc::Sender<AgentMessage>,
}
```

The `SreAgent::run` signature changes from:

```rust
async fn run(&self, inbox: Receiver<AgentMessage>, outbox: Sender<AgentMessage>) -> Result<(), AgentError>;
```

To:

```rust
async fn run(&self, inbox: Receiver<InboundMessage>) -> Result<(), AgentError>;
```

Each investigation creates its own `mpsc::channel` pair. The `reply_tx` is passed through the router to agents with each task. Agents reply through this channel, guaranteeing responses are scoped to the correct investigation.

### How It Works

1. `Orchestrator::investigate()` creates a `(reply_tx, reply_rx)` pair
2. When routing a message, the orchestrator passes `reply_tx.clone()` to the router
3. The router wraps the `AgentMessage` + `reply_tx` into an `InboundMessage` and sends it to the agent
4. The agent processes the task and sends its response via `inbound.reply_tx`
5. The orchestrator reads from its scoped `reply_rx` — only this investigation's responses appear

### Migration

- `AgentHandle.sender` type changes from `Sender<AgentMessage>` to `Sender<InboundMessage>`
- `AgentRouter::route_to_type/route_to_id` gains a `reply_tx` parameter
- `Orchestrator` no longer holds a shared inbox field
- All agent `run()` implementations updated to destructure `InboundMessage`

## Consequences

### What becomes easier

- **Concurrent investigations** — Each investigation has isolated I/O. No message interleaving.
- **Reasoning about correctness** — The reply channel type-checks the contract: agents can only respond to the investigation that asked.
- **Phase 2 readiness** — `max_concurrent_investigations` config can now be meaningfully enforced.
- **Testing** — Test harnesses create a reply channel per test, making assertions simpler.

### What becomes harder

- **Health checks** — Health check responses now go to whatever investigation dispatched the check. For global health monitoring, a separate mechanism (e.g., a dedicated health channel) will be needed. Acceptable for Phase 1 where health checks are per-investigation.
- **Agent API change** — All `SreAgent` implementations must be updated when this ADR is applied. The change is mechanical (destructure `InboundMessage` instead of using a separate outbox parameter).
- **Broadcast messages** — The `MessageTarget::Broadcast` variant becomes harder to implement since there's no global outbox. Deferred to Phase 2 when broadcast is actually needed.
