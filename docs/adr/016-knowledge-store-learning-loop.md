# ADR-016: Knowledge Store + Learning Loop

## Status

Accepted (2026-06-20). Second increment of the Phase 3 intelligence layer —
extends the `sre-intelligence` crate with a vector memory of past incidents and
a feedback loop, and wires both into the orchestrator.

## Context

[ADR-015](015-llm-reasoning-engine.md) added the `ReasoningEngine` and, with
deliberate foresight, a `related_incidents: Vec<String>` field on
`ReasoningInput` — **empty**, a seam to be filled by a knowledge store. This ADR
fills it.

The Phase 3 architecture diagram (`docs/diagrams/phase3-architecture.md`) places
two more boxes in the intelligence layer beside the reasoning engine:

- **Knowledge Store (Qdrant)** — a vector DB of past incidents. The reasoning
  engine reads it (`LLM <--> KNOW`): retrieval-augmented generation (RAG), so the
  RCA is grounded in what actually happened before, not just the current
  findings.
- **Learning Loop (Outcome Tracking)** — after an investigation/remediation, the
  platform observes the result, **embeds the incident** into the knowledge store
  (`LEARN --> embed incident --> KNOW`), and **updates confidence scores**
  (`LEARN --> update scores --> TRUST`). The diagram's *Learning Loop* panel
  defines the outcome decision tree (resolved within 15 min → success; else
  re-fired within 1 hr → failure; else success).

The design tension is identical to ADR-015 and resolved the same way: **a real
external dependency (Qdrant) vs. a deterministic, offline test/dev path.** RAG
must work in CI and in dev without standing up a vector DB.

A second, subtler tension is **embeddings**. Qdrant stores and compares vectors
but does not produce them — text must be embedded first, classically by a hosted
model (Azure OpenAI `text-embedding-3-small`, etc.). That is one more external,
non-deterministic, paid dependency. The platform's offline-testability bar
applies to embedding too.

## Options Considered

| Option | Pros | Cons |
|--------|------|------|
| **A. `KnowledgeStore` trait + deterministic mock + real Qdrant binding, config-selected; local lexical embedder with a seam for a semantic one** (chosen) | Same trait + mock + real-binding shape as `auth_mode`/`ReasoningEngine`. CI/dev run an in-memory cosine store with a dependency-free lexical embedder — RAG works fully offline and is reproducible. Qdrant activates by config. Embedding is abstracted behind an `Embedder` trait, so a semantic model is a drop-in with no store change. Best-effort: a down store yields no grounding and never blocks an investigation. | The default lexical embedder captures term overlap, not deep semantics — good for "same resource / same error class," weaker for paraphrase. Two store paths to maintain. |
| B. Qdrant only, hosted embeddings | "Real" semantic RAG always. | Breaks CI (no Qdrant, no embedding key, non-deterministic, paid per call). Couples every investigation to two external services. Fails the offline bar. |
| C. Postgres `pgvector` instead of Qdrant | Reuses the existing Postgres. | Adds a DB extension + migration; the Phase 3 diagram specifies Qdrant; still needs an embedder. Doesn't remove the core tension, just moves the vector store. Revisit only if operating two datastores proves costly. |
| D. No vector store — keyword search past incidents in Postgres | Simple, no new dependency. | Not what the diagram calls for; keyword recall is poor for "similar incident"; doesn't generalize to semantic embeddings later. |

## Decision

Adopt **Option A**. Three additions to `sre-intelligence`, mirroring the ADR-015
shape, plus orchestrator wiring.

### 1. `Embedder` trait + `HashEmbedder`

```rust
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
}
```

`HashEmbedder` is the **feature-hashing trick**: tokenize → hash each token
(FNV-1a, stable across platforms and Rust versions, unlike the std hasher) into a
signed bucket → L2-normalize, so cosine similarity is a dot product. It is a
*lexical* embedding (term overlap), fully deterministic and dependency-free. A
semantic embedder (Azure OpenAI) is a future `Embedder` impl — the store doesn't
change.

### 2. `KnowledgeStore` trait + `MockKnowledgeStore` + `QdrantKnowledgeStore`

```rust
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    fn name(&self) -> &str;
    async fn ensure_ready(&self) -> Result<(), KnowledgeError> { Ok(()) }
    async fn record(&self, incident: &IncidentRecord) -> Result<(), KnowledgeError>;
    async fn search(&self, query: &IncidentQuery, k: usize)
        -> Result<Vec<RelatedIncident>, KnowledgeError>;
}
```

- `MockKnowledgeStore` — in-memory `Vec` of `(vector, record)`, ranked by cosine.
  Deterministic, no network — the CI/dev default and a genuinely functional RAG
  backend for demos.
- `QdrantKnowledgeStore` — raw-HTTP REST binding (create-collection, upsert,
  search) via `reqwest`, embedding locally via the `Embedder`. The body builders
  and the response parser are **pure functions, unit-tested with no network**
  (same pattern as `ClaudeReasoningEngine`). The investigation UUID is the point
  id, so re-recording upserts rather than duplicates.
- `build_knowledge_store(&KnowledgeConfig)` returns `None` when disabled; Qdrant
  when `provider = "qdrant"`, else the mock — exactly like `auth_mode`/reasoning.

### 3. `LearningLoop`

Closes the feedback cycle. `OutcomeSignals::classify()` implements the diagram's
decision tree as a **pure function** (no remediation → `Inconclusive`; resolved
in window → `Success`; else re-fired in the hour → `Failure`; else `Success`).
`record_outcome()` stamps the outcome, updates a per-category `ConfidenceTracker`
(the success-rate signal the progressive-trust model will consume), and embeds
the incident into the knowledge store. The crate stays **clock-free** — callers
stamp timestamps — so its tests need no wall clock.

### 4. Orchestrator wiring

The orchestrator gains two optional fields (`with_knowledge`, `with_learning`),
both `None` by default. In `investigate()`, after the report is generated:

1. **Retrieve** (RAG): when a reasoning engine is configured, query the knowledge
   store for the `top_k` most-similar past incidents and pass their grounding
   lines as `ReasoningInput.related_incidents` — the seam from ADR-015 is now
   populated.
2. **Reason**: unchanged from ADR-015, now grounded.
3. **Record**: embed the completed incident via the learning loop. This is the
   read-only investigation path, so the outcome classifies as `Inconclusive`
   (nothing was remediated here); the remediation pipeline records real
   success/failure outcomes separately in a later increment.

All three steps are **best-effort**: a store error logs and degrades to the
ADR-015 (ungrounded) behavior, which itself degrades to the Phase 1/2 rule-based
summary. Nothing here can fail or block an investigation. Disabled by default
(`knowledge.enabled = false`).

## Consequences

### Positive

- The `related_incidents` seam is filled: the RCA is now grounded in similar past
  incidents (RAG), not just the current findings.
- The platform *learns*: every investigation is embedded, so the corpus grows and
  retrieval improves over time; outcomes feed a per-category confidence signal.
- Offline-first preserved: the in-memory mock store + lexical embedder give
  reproducible RAG in CI/dev with zero external dependencies. Qdrant and a
  semantic embedder are independent, config/trait-level opt-ins.
- Provider- and embedding-agnostic: Qdrant↔pgvector and lexical↔semantic are each
  swappable behind a trait without touching the orchestrator.
- The `ConfidenceTracker` success-rate is exactly the input the progressive-trust
  model (next increment) needs — that seam is now in place, mirroring how ADR-015
  pre-placed `related_incidents`.

### Negative

- The default lexical embedder is not semantic; paraphrased-but-different
  incidents may under-retrieve. Acceptable for "same resource / same error class"
  recall; a semantic `Embedder` is the upgrade path.
- Two store implementations and an embedding abstraction to maintain.
- The learning loop currently embeds at investigation time with an `Inconclusive`
  outcome on the read-only path; closing the loop with *real* post-remediation
  outcomes (the 15-min/1-hr monitor) is wired structurally but the monitor that
  produces those signals lands with auto-remediation.
- Qdrant payloads duplicate a little incident data already in Postgres; the
  vector store is a derived index, not the system of record (Postgres remains
  authoritative).

### Follow-ups

- Semantic `Embedder` impl (Azure OpenAI) behind the same trait.
- Post-remediation outcome monitor to feed real `OutcomeSignals` (resolved-in-15,
  refired-in-1hr) — pairs with the auto-remediation engine.
- Progressive-trust model consuming `ConfidenceTracker.success_rate` to adjust
  auto-approve thresholds (next increment).
- Optional: migrate the vector store to `pgvector` if running a second datastore
  proves operationally costly.

## References

- [ADR-015: LLM Reasoning Engine](015-llm-reasoning-engine.md) — pre-placed the
  `related_incidents` RAG seam this ADR fills; same trait + mock + real-binding shape.
- [ADR-004: Read-Only Phase 1 Safety Model](004-read-only-phase1-safety-model.md)
  — the knowledge store and learning loop are read-only with respect to Azure;
  they observe and remember, they do not act.
- [ADR-009: Remediation Safety Model](009-remediation-safety-model.md) — the
  learning loop's confidence signal informs the progressive-trust model but never
  bypasses the approval gate.
- `docs/diagrams/phase3-architecture.md` — the *Learning Loop* and intelligence-
  layer topology this ADR implements.
- Qdrant REST API (`/collections`, `/points`, `/points/search`) — the binding target.
