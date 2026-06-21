use std::sync::Arc;

use async_trait::async_trait;
use tracing::{info, warn};

use sre_common::config::ReasoningConfig;
use sre_common::findings::Finding;

use crate::claude::ClaudeReasoningEngine;
use crate::mock::MockReasoningEngine;

/// Input to the reasoning engine: the alert context plus the agents' structured
/// findings. `related_incidents` is populated by the knowledge store (Phase 3
/// RAG) — empty until then.
#[derive(Debug, Clone)]
pub struct ReasoningInput {
    pub investigation_id: String,
    pub affected_resource: String,
    pub trigger: String,
    pub severity: String,
    pub findings: Vec<Finding>,
    pub agent_summary: String,
    pub overall_confidence: f64,
    /// Retrieved past-incident summaries for grounding (Phase 3 knowledge store).
    pub related_incidents: Vec<String>,
}

/// The synthesized root-cause analysis.
#[derive(Debug, Clone)]
pub struct ReasoningOutput {
    pub root_cause: String,
    pub confidence: f64,
    pub contributing_factors: Vec<String>,
    pub recommended_actions: Vec<String>,
    /// Human-readable RCA narrative.
    pub narrative: String,
    /// Which engine produced this: `"mock"` | `"claude"`.
    pub engine: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ReasoningError {
    #[error("reasoning HTTP error: {0}")]
    Http(String),
    #[error("reasoning provider rejected request: {status} {message}")]
    Provider { status: u16, message: String },
    #[error("failed to parse reasoning response: {0}")]
    Parse(String),
}

/// Synthesizes investigation findings into a root-cause analysis.
///
/// Implementations MUST be best-effort and side-effect-free with respect to the
/// platform — a failure returns an error and the orchestrator falls back to the
/// rule-based summary; it never blocks an investigation from completing.
#[async_trait]
pub trait ReasoningEngine: Send + Sync {
    /// Engine name, for logging (`"mock"` | `"claude"`).
    fn name(&self) -> &str;

    async fn synthesize(&self, input: &ReasoningInput) -> Result<ReasoningOutput, ReasoningError>;
}

/// Build the reasoning engine from config (mirrors `auth_mode` selection).
/// Returns `None` when reasoning is disabled. When `provider = "claude"` and the
/// API key env var is set, returns the real Claude engine; otherwise (or on a
/// missing key) falls back to the deterministic mock so CI/dev always work.
pub fn build_engine(cfg: &ReasoningConfig) -> Option<Arc<dyn ReasoningEngine>> {
    if !cfg.enabled {
        return None;
    }

    match cfg.provider.as_str() {
        "claude" => match std::env::var(&cfg.api_key_env) {
            Ok(key) if !key.is_empty() => {
                info!(model = %cfg.model, "Reasoning engine: Claude (Anthropic Messages API)");
                Some(Arc::new(ClaudeReasoningEngine::new(
                    key,
                    cfg.model.clone(),
                    cfg.max_tokens,
                    cfg.base_url.clone(),
                )))
            }
            _ => {
                warn!(
                    env = %cfg.api_key_env,
                    "Reasoning provider is 'claude' but {} is not set — falling back to mock reasoning engine",
                    cfg.api_key_env
                );
                Some(Arc::new(MockReasoningEngine::new()))
            }
        },
        other => {
            if other != "mock" {
                warn!(provider = %other, "Unknown reasoning provider — using mock");
            } else {
                info!("Reasoning engine: deterministic mock");
            }
            Some(Arc::new(MockReasoningEngine::new()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::config::ReasoningConfig;

    fn cfg(enabled: bool, provider: &str, api_key_env: &str) -> ReasoningConfig {
        ReasoningConfig {
            enabled,
            provider: provider.to_string(),
            model: "claude-opus-4-8".to_string(),
            max_tokens: 1024,
            api_key_env: api_key_env.to_string(),
            base_url: None,
        }
    }

    #[test]
    fn disabled_returns_none() {
        assert!(build_engine(&cfg(false, "mock", "UNUSED")).is_none());
    }

    #[test]
    fn mock_provider_builds_mock() {
        let engine = build_engine(&cfg(true, "mock", "UNUSED")).unwrap();
        assert_eq!(engine.name(), "mock");
    }

    #[test]
    fn unknown_provider_falls_back_to_mock() {
        let engine = build_engine(&cfg(true, "totally-unknown", "UNUSED")).unwrap();
        assert_eq!(engine.name(), "mock");
    }

    #[test]
    fn claude_without_api_key_falls_back_to_mock() {
        // An env var name that is guaranteed not to be set.
        let engine =
            build_engine(&cfg(true, "claude", "REASONING_KEY_DEFINITELY_UNSET_8f3a")).unwrap();
        assert_eq!(engine.name(), "mock");
    }

    #[test]
    fn claude_with_api_key_builds_claude() {
        // Unique var name so this test owns it (no collision with other tests).
        let var = "REASONING_KEY_FOR_BUILD_ENGINE_TEST_42";
        std::env::set_var(var, "sk-ant-test");
        let engine = build_engine(&cfg(true, "claude", var)).unwrap();
        assert_eq!(engine.name(), "claude");
        std::env::remove_var(var);
    }

    #[test]
    fn reasoning_error_display_includes_context() {
        assert!(ReasoningError::Http("boom".into()).to_string().contains("boom"));
        assert!(ReasoningError::Provider {
            status: 429,
            message: "slow down".into(),
        }
        .to_string()
        .contains("429"));
        assert!(ReasoningError::Parse("bad json".into())
            .to_string()
            .contains("bad json"));
    }
}
