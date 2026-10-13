# ADR-015: LLM Reasoning Engine

## Status

Accepted (2026-06-20). First implementation of the Phase 3 intelligence layer —
the `sre-intelligence` crate and its wiring into the orchestrator.

## Context

Phases 1–2 built a platform that *gathers* evidence: agents query Azure, the
orchestrator aggregates findings, and a rule-based `FindingAggregator` produces
a severity-weighted confidence and a templated summary. That summary is a
concatenation of what each agent found — it does not *reason* across the
findings to name a single most-likely root cause, rank contributing factors, or
recommend a specific course of action. An on-call engineer still has to do the
synthesis.

Phase 3's goal is to close that last gap: turn the read-only investigation
platform into an **intelligent** one that produces a genuine root-cause analysis
(RCA). The phase-3 architecture diagram places a "Reasoning Engine (Claude /
Azure OpenAI)" at the center of the intelligence layer, downstream of the agents
and upstream of remediation and the knowledge store.

The design tension is the same one the platform has resolved everywhere else
(auth, ARM writes, notifications): **a real external dependency vs. a
deterministic test/dev path.** An LLM call is non-deterministic, costs money,
needs a credential, and is unavailable in CI. The rest of the platform must
build, test, and run without it.

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. Trait + deterministic mock + real Claude binding, config-selected** (chosen) | Matches the platform's `TokenProvider`/`auth_mode` pattern exactly. CI and dev run the deterministic mock; production runs Claude when a key is present. Reasoning is opt-in and isolated behind one trait, so the provider can change (Azure OpenAI, a local model) without touching the orchestrator. Best-effort: a failed LLM call falls back to the rule-based summary and never blocks an investigation. | Two code paths to maintain (mock + real). The mock's "reasoning" is rule-based, not intelligent — it exists for determinism, not insight. |
| B. Always call a real LLM | Simplest single path; always "intelligent". | Breaks CI (no key, non-deterministic, costs tokens per test). Couples every investigation to an external dependency's availability. Unacceptable given the platform's offline-testability bar. |
| C. No LLM — extend the rule-based aggregator | Fully deterministic, free, offline. | Doesn't meet the Phase 3 goal. Rules can weight and rank, but they can't synthesize a narrative root cause across heterogeneous findings. |
| D. Anthropic SDK | Idiomatic if it existed. | **There is no official Anthropic SDK for Rust.** The supported path for an unsupported language is raw HTTP against the Messages API. |

## Decision

Adopt **Option A**. A `ReasoningEngine` trait in the new `sre-intelligence`
crate, with two implementations selected by config (mirroring `auth_mode`):

### 1. The trait

```rust
#[async_trait]
pub trait ReasoningEngine: Send + Sync {
    fn name(&self) -> &str;
    async fn synthesize(&self, input: &ReasoningInput) -> Result<ReasoningOutput, ReasoningError>;
}
```

- `ReasoningInput` carries the alert context + the agents' structured `Finding`s
  + the rule-based agent summary. It also carries `related_incidents: Vec<String>`
  — empty for now, populated by the Phase 3 knowledge store (RAG) in a later ADR.
- `ReasoningOutput` is the RCA: `root_cause`, `confidence`, `contributing_factors`,
  `recommended_actions`, `narrative`, and which `engine` produced it.

### 2. `MockReasoningEngine` (deterministic)

Rule-based synthesis: the most severe finding drives the root cause, finding
suggestions become recommended actions, the narrative is templated. No network,
reproducible. Used in CI, in dev without an API key, and as the runtime fallback.

### 3. `ClaudeReasoningEngine` (real)

A raw-HTTP binding to the Anthropic **Messages API** (`POST /v1/messages`,
`x-api-key` + `anthropic-version: 2023-06-01`), via `reqwest` — there is no Rust
SDK. Defaults to **`claude-opus-5-5`** (since 2026-10-13; `claude-opus-4-8` before, which the three recorded eval runs used). A system prompt frames Claude as a senior
SRE and constrains the response to a JSON RCA schema; the response's text block
is parsed into `ReasoningOutput` (tolerating markdown fences and non-JSON output
via a fallback). The request-building and response-parsing are pure functions,
unit-tested without a network call.

### 4. Selection + wiring

`build_engine(&ReasoningConfig)` returns `None` when reasoning is disabled;
otherwise it returns the Claude engine when `provider = "claude"` **and** the API
key env var (`ANTHROPIC_API_KEY` by default) is set, and the mock engine
otherwise. The orchestrator holds an optional engine (`with_reasoning`) and, at
the end of `investigate()`, **enriches the report summary** with the synthesized
RCA — best-effort: on any error it logs and keeps the rule-based summary. The
investigation result, audit trail, and state machine are unchanged; reasoning is
purely additive and opt-in (`reasoning.enabled = false` by default).

## Consequences

### Positive

- The platform now produces a real RCA — named root cause, ranked factors,
  recommended actions, narrative — not just a concatenation of findings.
- Provider-agnostic behind one trait: Azure OpenAI or a self-hosted model is a
  new `ReasoningEngine` impl plus a `provider` value, no orchestrator change.
- CI and dev are unaffected: the deterministic mock runs with zero external
  dependencies; the Claude path activates only when a key is present.
- Best-effort + opt-in means zero risk to the existing investigation flow — a
  reasoning failure or a missing key degrades to the Phase 1/2 behavior.
- The `related_incidents` seam is already in `ReasoningInput`, so the knowledge
  store (next ADR) plugs in as retrieval-augmented grounding without a signature
  change.

### Negative

- The mock engine's output is not genuinely intelligent — it's a deterministic
  stand-in. Tests assert structure and rules, not insight.
- The Claude path costs tokens per investigation (Opus-tier) and adds latency to
  the tail of `investigate()`. Operators tune `model`/`max_tokens` or run the
  mock; a future enhancement is to gate LLM synthesis on severity.
- Raw-HTTP binding means we own the request/response shapes and must track
  Messages API changes ourselves (the supported approach absent a Rust SDK).
- The model is asked to return JSON; non-conforming output falls back to a
  narrative-only RCA. Structured-output enforcement is a possible refinement.

### Follow-ups

- Knowledge store (Qdrant) populates `related_incidents` for RAG-grounded RCA.
- Learning loop feeds investigation outcomes back to adjust confidence.
- Optional severity gate so only Sev0–Sev2 incidents pay for LLM synthesis.

## References

- [ADR-004: Read-Only Phase 1 Safety Model](004-read-only-phase1-safety-model.md) — the reasoning engine is read-only; it synthesizes, it does not act.
- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — Phase 3 LLM reasoning is explicitly *outside* the remediation auto-approve envelope; the RCA informs proposals but the gate still decides.
- `docs/diagrams/phase3-architecture.md` — the intelligence-layer topology this ADR begins to implement.
- Anthropic Messages API (`POST /v1/messages`) — the binding target.
