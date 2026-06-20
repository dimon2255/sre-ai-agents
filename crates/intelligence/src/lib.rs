//! Intelligence layer (Phase 3) — LLM-powered reasoning over investigation
//! findings.
//!
//! Mirrors the platform's trait + mock + real-binding philosophy (like
//! `TokenProvider` / `auth_mode`): a [`ReasoningEngine`] trait with a
//! deterministic [`MockReasoningEngine`] for CI/dev and a real
//! [`ClaudeReasoningEngine`] (Anthropic Messages API) activated by config +
//! `ANTHROPIC_API_KEY`. The engine synthesizes the agents' structured findings
//! into a ranked root-cause analysis — the step that turns the read-only
//! investigation platform into an *intelligent* one.

pub mod claude;
pub mod mock;
pub mod reasoning;

pub use claude::ClaudeReasoningEngine;
pub use mock::MockReasoningEngine;
pub use reasoning::{
    build_engine, ReasoningEngine, ReasoningError, ReasoningInput, ReasoningOutput,
};
