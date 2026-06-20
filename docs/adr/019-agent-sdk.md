# ADR-019: Agent SDK

## Status

Accepted (2026-06-20). Fifth increment of the Phase 3 intelligence layer — opens
the platform to third-party agents.

## Context

The Phase 3 architecture diagram shows **Custom Agents (via Agent SDK)** sitting
alongside the first-party investigation agents (Triage, Logs, Metrics, Trace,
Infra) on the orchestrator's bus. The Phase 3 exit criteria call for an "Agent SDK
documented + 1 custom agent built by another team." The goal is extensibility:
another team should be able to add a domain-specific agent (disk, Kubernetes, a
bespoke data source) without forking the platform.

Today the `SreAgent` trait lives in `sre-agents` — the crate that also contains
the concrete agents and therefore depends on `sre-azure` (the Azure query client,
KQL templates, ARM). A third party implementing `SreAgent` from there would pull
in the entire Azure layer and the platform's internals as a transitive
dependency, even though the contract itself needs none of it: the trait references
only `sre-common` types (`AgentId`, `AgentType`, `InboundMessage`, `AgentError`).

There is also a smaller gap: `AgentType` is a closed enum (Triage/Logs/Metrics/
Trace/Infra/Remediation) with no way to name a third-party agent.

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Extract the contract into a lightweight `sre-agent-sdk` crate (sre-common only); `sre-agents` re-exports it; add `AgentType::Custom(String)`** (chosen) | Third parties depend on a small, stable surface — the trait, the message/finding types, a `prelude`, docs, and a runnable example — with no Azure/store/orchestrator baggage. First-party agents are just one consumer (they re-export, so every `sre_agents::SreAgent` path is unchanged). Custom agents get a real identity. | One more crate. The contract is now a published surface, so changing it is a breaking change (which is the point — it should be stable). |
| B. Leave the trait in `sre-agents`; document it as the SDK | No move. | Third parties inherit `sre-azure` + the platform internals transitively; the "SDK" is heavyweight and coupled; the dependency direction is backwards (extensions depend on the whole platform). |
| C. New crate that *re-exports* from `sre-agents` | Single facade. | Still pulls `sre-agents` → `sre-azure` transitively; doesn't solve the weight problem, just hides it. |
| D. Generary trait objects / dynamic plugin loading (`dlopen`) | "True" plugins. | Massive complexity (ABI stability, unsafe, versioning) for no benefit over compiling an agent in; Rust's story here is a source-level SDK, not binary plugins. |

## Decision

Adopt **Option A**.

### 1. `sre-agent-sdk` crate

A new crate depending only on `sre-common`, `tokio`, and `async-trait`. It owns:

- The **`SreAgent`** trait and **`AgentHandle`** (moved verbatim from
  `sre-agents`).
- A **`prelude`** re-exporting everything an agent author needs in one glob
  import — the trait, `async_trait`, `mpsc`, and the `sre-common` message/finding
  types (`AgentMessage`, `AgentPayload`, `AnalysisTask`, `AnalysisResult`,
  `Finding`, `AgentHealth`, …).
- Crate-level **documentation** of the contract (one task → one reply, run until
  the inbox closes, answer health checks, stay read-only) plus a **runnable
  example** (`examples/custom_agent.rs`) that wires the channels and drives a
  custom `DiskSpaceAgent` end-to-end, and a **README** guide.

### 2. `sre-agents` re-exports the contract

`sre-agents` depends on `sre-agent-sdk` and re-exports the trait + handle
(`crates/agents/src/traits.rs` becomes a one-line `pub use`). Every existing
path — `sre_agents::SreAgent`, `crate::traits::SreAgent`, `AgentHandle` in the
router/orchestrator/bootstrap — is unchanged. The dependency direction is now
correct: the SDK is the leaf, the platform's agents and the host build on it.

### 3. `AgentType::Custom(String)`

A new variant gives third-party agents an identity for routing keys and display
(`Display` renders the inner kind). It is additive: the router compares
`AgentType` by equality (no `Hash`, no exhaustive match to break), and the
orchestrator's default task fan-out already has a catch-all, so a custom agent
simply isn't auto-dispatched a built-in `AnalysisTaskType` — it is driven by a
triage recommendation or a broadcast, which is the host's extension point.

## Consequences

### Positive

- A third party adds an agent by depending on one small crate and implementing one
  trait — no Azure layer, no store, no orchestrator in their dependency tree.
- The contract is now an explicit, documented, versioned surface (trait + prelude +
  example + README), which is what an SDK should be.
- Zero churn for existing code: `sre-agents` re-exports, so all internal paths and
  the 19 agent tests pass unchanged.
- Custom agents have a first-class identity (`AgentType::Custom`).
- The example is compile-checked in CI (it builds as part of the workspace) and is
  runnable (`cargo run --example custom_agent`), so the docs can't rot.

### Negative

- One more crate in the workspace.
- The SDK surface is now a stability commitment — changing `SreAgent` or the
  re-exported types is a breaking change for downstream agents (acceptable, and
  the reason to keep the surface small).
- Full custom-agent *routing* (triage learning to recommend a new kind, a custom
  `AnalysisTaskType`) is left as a host extension point; the SDK delivers the
  authoring contract, not an auto-discovery mechanism.

### Follow-ups

- Publish `sre-agent-sdk` to a registry (or tag a git version) once the surface
  settles, so external teams consume a pinned version rather than a path.
- A capability-driven dispatch path so triage can recommend custom agents by
  capability string without a code change.
- A second worked example against a real external data source (e.g. Prometheus).

## References

- [ADR-003: Event-Driven Agent Communication](003-event-driven-agent-communication.md) — the message envelope the SDK exposes.
- [ADR-006: Per-Investigation Reply Channels](006-per-investigation-reply-channels.md) — the reply contract agents must honor.
- [ADR-004: Read-Only Phase 1 Safety Model](004-read-only-phase1-safety-model.md) — investigation agents observe, they don't mutate.
- `docs/diagrams/phase3-architecture.md` — the "Custom Agents (via Agent SDK)" node this implements.
