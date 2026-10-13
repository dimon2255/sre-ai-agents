use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tracing::debug;

use crate::reasoning::{ReasoningEngine, ReasoningError, ReasoningInput, ReasoningOutput};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";

/// Real reasoning engine backed by the Anthropic Messages API
/// (`POST /v1/messages`). Rust has no official Anthropic SDK, so this is a raw
/// HTTP binding via `reqwest`. Activated by config + `ANTHROPIC_API_KEY`;
/// defaults to `claude-opus-5-5`.
pub struct ClaudeReasoningEngine {
    http: reqwest::Client,
    api_key: String,
    model: String,
    max_tokens: u32,
    base_url: String,
}

impl ClaudeReasoningEngine {
    pub fn new(api_key: String, model: String, max_tokens: u32, base_url: Option<String>) -> Self {
        Self {
            http: sre_common::http::client_builder()
                // Opus 5.5 thinks before it answers, so a synthesis can run well past the
                // 60 seconds that fitted Opus 4.8; the eval judge already allows 300.
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .expect("failed to build HTTP client"),
            api_key,
            model,
            max_tokens,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        }
    }

    fn system_prompt() -> &'static str {
        "You are a senior Site Reliability Engineer performing root-cause analysis. \
         You receive an Azure alert and the structured findings produced by automated \
         investigation agents (logs, metrics, traces, infrastructure changes). \
         Synthesize the single most likely root cause, the contributing factors, and \
         concrete recommended actions. Be specific and grounded in the findings; do not \
         invent signals that are not present. \
         Respond with ONLY a JSON object, no prose and no markdown fences, matching this schema: \
         {\"root_cause\": string, \"confidence\": number between 0 and 1, \
         \"contributing_factors\": string[], \"recommended_actions\": string[], \
         \"narrative\": string}."
    }

    /// Render the user message — the alert + findings serialized as JSON for the
    /// model to reason over. Pure and unit-testable.
    pub fn user_content(input: &ReasoningInput) -> String {
        let findings: Vec<serde_json::Value> = input
            .findings
            .iter()
            .map(|f| {
                json!({
                    "category": format!("{:?}", f.category),
                    "severity": f.severity.to_string(),
                    "title": f.title,
                    "description": f.description,
                    "confidence": f.confidence,
                    "source_agent": f.source_agent,
                    "suggestions": f.suggestions,
                })
            })
            .collect();

        let payload = json!({
            "alert": input.trigger,
            "affected_resource": input.affected_resource,
            "severity": input.severity,
            "agent_summary": input.agent_summary,
            "overall_confidence": input.overall_confidence,
            "findings": findings,
            "related_past_incidents": input.related_incidents,
        });

        format!(
            "Analyze this incident and return your root-cause analysis as JSON only.\n\n{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        )
    }

    /// Build the Anthropic Messages API request body. Pure and unit-testable.
    pub fn build_request_body(&self, input: &ReasoningInput) -> serde_json::Value {
        json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "system": Self::system_prompt(),
            "messages": [
                { "role": "user", "content": Self::user_content(input) }
            ],
        })
    }

    /// Parse the model's text block into a [`ReasoningOutput`]. Tolerates
    /// markdown code fences and non-JSON output (falls back to using the raw
    /// text as the narrative). Pure and unit-testable.
    pub fn parse_response(text: &str) -> ReasoningOutput {
        let cleaned = strip_code_fence(text);
        match serde_json::from_str::<ReasoningWire>(cleaned) {
            Ok(w) => ReasoningOutput {
                root_cause: if w.root_cause.is_empty() {
                    "See narrative".to_string()
                } else {
                    w.root_cause
                },
                confidence: w.confidence.clamp(0.0, 1.0),
                contributing_factors: w.contributing_factors,
                recommended_actions: w.recommended_actions,
                narrative: if w.narrative.is_empty() {
                    text.to_string()
                } else {
                    w.narrative
                },
                engine: "claude".to_string(),
            },
            Err(_) => ReasoningOutput {
                root_cause: "Unstructured model response".to_string(),
                confidence: 0.5,
                contributing_factors: vec![],
                recommended_actions: vec![],
                narrative: text.to_string(),
                engine: "claude".to_string(),
            },
        }
    }
}

#[async_trait]
impl ReasoningEngine for ClaudeReasoningEngine {
    fn name(&self) -> &str {
        "claude"
    }

    async fn synthesize(&self, input: &ReasoningInput) -> Result<ReasoningOutput, ReasoningError> {
        let url = format!("{}/v1/messages", self.base_url);
        debug!(model = %self.model, "Calling Anthropic Messages API for RCA synthesis");

        let resp = self
            .http
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .json(&self.build_request_body(input))
            .send()
            .await
            .map_err(|e| ReasoningError::Http(e.to_string()))?;

        let status = resp.status();
        if !status.is_success() {
            let message = resp.text().await.unwrap_or_default();
            return Err(ReasoningError::Provider {
                status: status.as_u16(),
                message,
            });
        }

        let body: AnthropicResponse = resp
            .json()
            .await
            .map_err(|e| ReasoningError::Parse(e.to_string()))?;

        // Opus 5.5 thinks before it answers, so the response begins with a thinking
        // block (empty text by default) before the text block; take the first text block.
        let text = body
            .content
            .iter()
            .find(|b| b.block_type == "text")
            .and_then(|b| b.text.clone())
            .ok_or_else(|| ReasoningError::Parse("no text block in response".to_string()))?;

        Ok(Self::parse_response(&text))
    }
}

#[derive(Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicBlock>,
}

#[derive(Deserialize)]
struct AnthropicBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize)]
struct ReasoningWire {
    #[serde(default)]
    root_cause: String,
    #[serde(default)]
    confidence: f64,
    #[serde(default)]
    contributing_factors: Vec<String>,
    #[serde(default)]
    recommended_actions: Vec<String>,
    #[serde(default)]
    narrative: String,
}

/// Strip a leading ```json / ``` fence and trailing ``` if the model wrapped
/// its JSON in markdown despite instructions.
fn strip_code_fence(text: &str) -> &str {
    let t = text.trim();
    let t = t
        .strip_prefix("```json")
        .or_else(|| t.strip_prefix("```"))
        .unwrap_or(t);
    let t = t.strip_suffix("```").unwrap_or(t);
    t.trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::findings::{Finding, FindingCategory, FindingSeverity};

    fn engine() -> ClaudeReasoningEngine {
        ClaudeReasoningEngine::new(
            "test-key".to_string(),
            "claude-opus-5-5".to_string(),
            2048,
            None,
        )
    }

    fn input() -> ReasoningInput {
        ReasoningInput {
            investigation_id: "id".to_string(),
            affected_resource: "payments-api".to_string(),
            trigger: "High Error Rate".to_string(),
            severity: "Sev1".to_string(),
            findings: vec![Finding {
                category: FindingCategory::ErrorSpike,
                severity: FindingSeverity::Critical,
                title: "500 spike".to_string(),
                description: "many 500s".to_string(),
                confidence: 0.9,
                observed_at: None,
                source_agent: "logs-agent".to_string(),
                evidence: vec![],
                suggestions: vec!["restart".to_string()],
            }],
            agent_summary: "errors".to_string(),
            overall_confidence: 0.85,
            related_incidents: vec![],
        }
    }

    #[test]
    fn request_body_has_model_system_and_user_message() {
        let body = engine().build_request_body(&input());
        assert_eq!(body["model"], "claude-opus-5-5");
        assert_eq!(body["max_tokens"], 2048);
        assert!(body["system"].as_str().unwrap().contains("root-cause"));
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("payments-api"));
        assert!(content.contains("500 spike"));
    }

    #[test]
    fn parse_response_reads_valid_json() {
        let text = r#"{"root_cause":"deploy regression","confidence":0.9,
            "contributing_factors":["error spike"],"recommended_actions":["rollback"],
            "narrative":"A recent deploy caused 500s."}"#;
        let out = ClaudeReasoningEngine::parse_response(text);
        assert_eq!(out.root_cause, "deploy regression");
        assert_eq!(out.confidence, 0.9);
        assert_eq!(out.recommended_actions, vec!["rollback"]);
        assert_eq!(out.engine, "claude");
    }

    #[test]
    fn parse_response_strips_code_fence() {
        let text = "```json\n{\"root_cause\":\"x\",\"confidence\":0.7}\n```";
        let out = ClaudeReasoningEngine::parse_response(text);
        assert_eq!(out.root_cause, "x");
        assert_eq!(out.confidence, 0.7);
    }

    #[test]
    fn parse_response_falls_back_on_non_json() {
        let text = "I think the database is down.";
        let out = ClaudeReasoningEngine::parse_response(text);
        assert_eq!(out.root_cause, "Unstructured model response");
        assert_eq!(out.narrative, "I think the database is down.");
    }

    #[test]
    fn parse_response_clamps_out_of_range_confidence() {
        let text = r#"{"root_cause":"x","confidence":2.5}"#;
        let out = ClaudeReasoningEngine::parse_response(text);
        assert_eq!(out.confidence, 1.0);
    }
}
