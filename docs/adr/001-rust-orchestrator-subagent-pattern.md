# ADR-001: Rust + Orchestrator-SubAgent Pattern

## Status

Accepted (2026-04-05)

## Context

We are building an AI-driven Site Reliability Engineering (SRE) platform that must:

- Coordinate multiple specialized agents to investigate production incidents
- Query telemetry data (logs, metrics, traces) from Azure services
- Propose and optionally execute remediation actions
- Operate reliably during incidents (when other systems may be degraded)
- Handle concurrent investigations without resource exhaustion
- Provide a production-safe, auditable decision trail

We need to choose a language, a coordination pattern, and a module boundary strategy for Phase 1.

### Options Considered

**Language:**

| Option | Pros | Cons |
|--------|------|------|
| **Rust** | Memory safety, zero-cost async, strong type system, single binary deployment, no GC pauses | Steeper learning curve, longer compile times, smaller ecosystem for AI/ML |
| Python | Fastest prototyping, rich AI/ML ecosystem | GIL limits concurrency, runtime errors, deployment complexity |
| Go | Fast compilation, good concurrency, simple deployment | Weaker type system, no sum types (enums), verbose error handling |

**Coordination Pattern:**

| Option | Pros | Cons |
|--------|------|------|
| **Orchestrator + Sub-Agents** | Clear responsibility split, centralized workflow state, controlled fan-out | Orchestrator is single coordination point |
| Peer-to-Peer Agents | No central bottleneck, agents self-organize | Hard to reason about, difficult to audit, emergent behavior is unpredictable |
| Pipeline (chain) | Simple linear flow | Too rigid for incident investigation which requires parallel analysis |

**Module Boundary (Phase 1):**

| Option | Pros | Cons |
|--------|------|------|
| **Modular Monolith** (Cargo workspace) | Single deployment, in-process communication, easy debugging | Must refactor for horizontal scaling |
| Microservices from day 1 | Independent scaling, team autonomy | Premature for small team, operational overhead, network latency between agents |

## Decision

- **Language**: Rust — memory safety and async concurrency are critical for a system that runs during incidents. The type system (enums, traits) maps naturally to agent messages and workflow states. Single binary simplifies deployment.
- **Pattern**: Orchestrator + Sub-Agents — the orchestrator owns workflow state and routing while sub-agents own domain expertise. This gives us auditability (all decisions flow through the orchestrator) and controlled concurrency.
- **Module Boundary**: Cargo workspace (modular monolith) for Phase 1. Each bounded context is a crate: `orchestrator`, `agents`, `azure`, `common`, `store`. Extractable to services in Phase 2+ if needed.

### Concurrency Model

- **Tokio async runtime** as the foundation
- Each agent runs as a long-lived `tokio::spawn` task with its own `mpsc::Receiver`
- The orchestrator holds `mpsc::Sender` handles to all agents
- `JoinSet` for parallel fan-out of investigation tasks
- `watch` channels for agent health/status monitoring

### Workspace Structure

```
sre-agents/
├── Cargo.toml                  # Workspace root
├── crates/
│   ├── common/                 # Shared types: messages, findings, config, errors
│   ├── store/                  # Persistence: traits + PostgreSQL (Phase 2a onward; was SQLite in Phase 1)
│   ├── azure/                  # Azure API clients: KQL, alerts, auth
│   ├── agents/                 # Agent implementations: triage, logs, metrics
│   └── orchestrator/           # Workflow engine, routing, aggregation, timeouts
├── src/
│   └── main.rs                 # CLI + webhook server entry point
└── config/
    └── default.toml            # Runtime configuration
```

## Consequences

### What becomes easier

- **Type-safe agent communication** — Rust enums enforce valid message payloads at compile time; malformed messages are impossible.
- **Fearless concurrency** — Tokio + mpsc channels give safe parallel agent execution without data races.
- **Single binary deployment** — One artifact to deploy, no runtime dependencies, minimal attack surface.
- **Auditability** — All agent interactions flow through the orchestrator, creating a natural audit trail.
- **Incremental extraction** — Crate boundaries can become service boundaries when scaling demands it.

### What becomes harder

- **Rapid prototyping** — Rust's compile times and borrow checker slow initial development compared to Python.
- **AI/ML integration** — The LLM reasoning layer (Phase 3) has a weaker Rust ecosystem; may need FFI or HTTP calls to Python services.
- **Team onboarding** — Rust has a steeper learning curve; new contributors need Rust proficiency.
- **Dynamic agent loading** — Modular monolith means adding a new agent requires recompilation. Phase 3's Agent SDK addresses this with a plugin trait.
