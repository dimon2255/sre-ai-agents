//! Intelligence layer (Phase 3) — LLM-powered reasoning over investigation
//! findings, grounded by a vector memory of past incidents and a feedback loop.
//!
//! Mirrors the platform's trait + mock + real-binding philosophy (like
//! `TokenProvider` / `auth_mode`):
//!
//! - [`ReasoningEngine`] — a deterministic [`MockReasoningEngine`] for CI/dev and
//!   a real [`ClaudeReasoningEngine`] (Anthropic Messages API) activated by
//!   config + `ANTHROPIC_API_KEY`. Synthesizes findings into a ranked root-cause
//!   analysis — the step that turns the read-only platform into an *intelligent* one.
//! - [`KnowledgeStore`] — a vector memory of past incidents with a deterministic
//!   in-memory [`MockKnowledgeStore`] and a real [`QdrantKnowledgeStore`]. Closes
//!   the `related_incidents` RAG seam: similar past incidents ground each RCA.
//! - [`LearningLoop`] — classifies post-action outcomes, embeds incidents back
//!   into the knowledge store, and tracks per-category confidence (the signal the
//!   progressive-trust model consumes).

pub mod claude;
pub mod embed;
pub mod knowledge;
pub mod learning;
pub mod mock;
pub mod qdrant;
pub mod reasoning;

pub use claude::ClaudeReasoningEngine;
pub use embed::{cosine, Embedder, HashEmbedder};
pub use knowledge::{
    build_knowledge_store, IncidentQuery, IncidentRecord, KnowledgeError, KnowledgeStore,
    MockKnowledgeStore, RelatedIncident,
};
pub use learning::{CategoryStat, ConfidenceTracker, LearningLoop, Outcome, OutcomeSignals};
pub use mock::MockReasoningEngine;
pub use qdrant::QdrantKnowledgeStore;
pub use reasoning::{
    build_engine, ReasoningEngine, ReasoningError, ReasoningInput, ReasoningOutput,
};
