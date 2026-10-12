//! Optional LLM tier (ADR-023 §6): the Claude reasoning engine writes a
//! root-cause analysis from each scenario's findings, and an LLM judge grades
//! it against the scenario's label.
//!
//! Costs money and is not deterministic, so it runs only with
//! `--engine claude` and never in the CI gate. Raw HTTP to the Messages API,
//! matching `sre_intelligence::ClaudeReasoningEngine` (Rust has no official SDK).

use anyhow::{bail, Context};
use serde_json::{json, Value};

use sre_intelligence::{ReasoningEngine, ReasoningInput, ReasoningOutput};

use crate::scenario::{Label, Scenario, NONE_LABEL};
use crate::score::{RcaGrade, RCA_ERROR_VERDICT};
use crate::Evaluated;

const ANTHROPIC_VERSION: &str = "2023-06-01";
const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
/// Server-side refusal fallback, `fallbacks: "default"` form.
const FALLBACK_BETA: &str = "server-side-fallback-2026-07-01";
pub const DEFAULT_JUDGE_MODEL: &str = "claude-opus-5-5";

const JUDGE_SYSTEM: &str = "You grade root-cause analyses written by an automated SRE \
investigation platform. You are given the ground truth for an incident (what actually \
happened and the true root cause) and the analysis the platform produced. Decide whether the \
analysis names the true root cause.\n\
- correct: the analysis's root cause is the same underlying cause as the ground truth. Naming \
the specific failing component (for example the deploy, the config change, the database, the \
partner API or the platform event) counts even if the wording differs. When the ground truth \
says there is no root cause to find, correct means the analysis says the evidence is \
insufficient, the signal is noise, or no actionable cause was found.\n\
- partial: the true cause appears only as a contributing factor, or the analysis hedges between \
several causes without choosing the true one.\n\
- incorrect: the analysis names a symptom (for example 'error spike' or 'high latency') or a \
different cause as the root cause, or asserts a cause when the ground truth says there is none.\n\
Judge only against the ground truth given. Do not reward plausible-sounding analysis that \
contradicts it.";

/// The grading call. Pure, so the request shape is unit-testable.
pub fn judge_request_body(model: &str, scenario: &Scenario, rca: &ReasoningOutput) -> Value {
    let label = Label::parse(&scenario.expected.root_cause);
    let truth = if label.matches(NONE_LABEL) {
        "none: there is no root cause the platform could or should name".to_string()
    } else {
        label.alternatives.join(" or ")
    };
    let user = format!(
        "## Ground truth\n\
         What happened: {}\n\
         True root cause category: {truth}\n\
         Why: {}\n\n\
         ## Analysis to grade\n\
         Root cause: {}\n\
         Contributing factors: {}\n\
         Narrative: {}",
        scenario.narrative,
        scenario.expected.rationale,
        rca.root_cause,
        if rca.contributing_factors.is_empty() {
            "(none)".to_string()
        } else {
            rca.contributing_factors.join("; ")
        },
        rca.narrative,
    );
    json!({
        "model": model,
        "max_tokens": 16000,
        "fallbacks": "default",
        "system": JUDGE_SYSTEM,
        "output_config": {
            "effort": "medium",
            "format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {
                        "verdict": {"type": "string", "enum": ["correct", "partial", "incorrect"]},
                        "reason": {"type": "string"}
                    },
                    "required": ["verdict", "reason"],
                    "additionalProperties": false
                }
            }
        },
        "messages": [{"role": "user", "content": user}]
    })
}

/// Extract `(verdict, reason)` from a Messages API response body.
pub fn parse_judge_response(body: &Value) -> anyhow::Result<(String, String)> {
    if body.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
        bail!(
            "judge declined the request: {}",
            body.get("stop_details").cloned().unwrap_or(Value::Null)
        );
    }
    let text = body
        .get("content")
        .and_then(Value::as_array)
        .and_then(|blocks| {
            blocks
                .iter()
                .find(|b| b.get("type").and_then(Value::as_str) == Some("text"))
        })
        .and_then(|b| b.get("text").and_then(Value::as_str))
        .context("judge response has no text block")?;
    let parsed: Value = serde_json::from_str(text).context("judge text is not JSON")?;
    let verdict = parsed
        .get("verdict")
        .and_then(Value::as_str)
        .context("judge JSON has no verdict")?
        .to_string();
    let reason = parsed
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    Ok((verdict, reason))
}

pub struct Judge {
    http: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl Judge {
    pub fn new(api_key: String, model: String, base_url: Option<String>) -> Self {
        Self {
            http: sre_common::http::client_builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("failed to build HTTP client"),
            api_key,
            model,
            base_url: base_url.unwrap_or_else(|| DEFAULT_BASE_URL.to_string()),
        }
    }

    pub async fn grade(
        &self,
        scenario: &Scenario,
        rca: &ReasoningOutput,
    ) -> anyhow::Result<(String, String)> {
        let resp = self
            .http
            .post(format!("{}/v1/messages", self.base_url))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("anthropic-beta", FALLBACK_BETA)
            .json(&judge_request_body(&self.model, scenario, rca))
            .send()
            .await
            .context("judge request failed")?;
        let status = resp.status();
        let text = resp.text().await.context("reading judge response")?;
        if !status.is_success() {
            bail!("judge request rejected: {status} {text}");
        }
        let body: Value = serde_json::from_str(&text).context("judge response is not JSON")?;
        parse_judge_response(&body)
    }
}

/// The input the orchestrator would hand the reasoning engine for this report.
pub fn reasoning_input(e: &Evaluated) -> ReasoningInput {
    let report = &e.outcome.report;
    ReasoningInput {
        investigation_id: report.investigation_id.clone(),
        affected_resource: report.affected_resource.clone(),
        trigger: report.trigger.clone(),
        severity: e.scenario.alert.severity.to_string(),
        findings: report.findings.clone(),
        agent_summary: report.summary.clone(),
        overall_confidence: report.overall_confidence,
        related_incidents: Vec::new(),
    }
}

/// Run the reasoning engine and the judge over every scenario, attaching a
/// grade to each score. A failed call is recorded as an incorrect grade with
/// the error as its reason, so one bad call does not discard the run.
pub async fn grade_all(evaluated: &mut [Evaluated], engine: &dyn ReasoningEngine, judge: &Judge) {
    for e in evaluated.iter_mut() {
        let grade = match engine.synthesize(&reasoning_input(e)).await {
            Ok(rca) => match judge.grade(&e.scenario, &rca).await {
                Ok((verdict, reason)) => RcaGrade {
                    engine: engine.name().to_string(),
                    root_cause: rca.root_cause,
                    correct: verdict == "correct",
                    verdict,
                    judge_reason: reason,
                },
                Err(err) => RcaGrade {
                    engine: engine.name().to_string(),
                    root_cause: rca.root_cause,
                    verdict: RCA_ERROR_VERDICT.to_string(),
                    correct: false,
                    judge_reason: format!("judge failed: {err:#}"),
                },
            },
            Err(err) => RcaGrade {
                engine: engine.name().to_string(),
                root_cause: String::new(),
                verdict: RCA_ERROR_VERDICT.to_string(),
                correct: false,
                judge_reason: format!("reasoning engine failed: {err}"),
            },
        };
        e.score.rca = Some(grade);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenario::load_dir;
    use sre_intelligence::ClaudeReasoningEngine;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

    fn text_response(text: &str) -> Value {
        json!({
            "content": [
                {"type": "thinking", "thinking": ""},
                {"type": "text", "text": text}
            ],
            "stop_reason": "end_turn"
        })
    }

    /// Stands in for the Messages API: answers the judge (structured output
    /// requested) and the reasoning engine differently.
    struct StubAnthropic;

    impl Respond for StubAnthropic {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("output_config").is_some() {
                ResponseTemplate::new(200).set_body_json(text_response(
                    r#"{"verdict":"correct","reason":"names the deploy"}"#,
                ))
            } else {
                ResponseTemplate::new(200).set_body_json(text_response(
                    r#"{"root_cause":"Revision deployed at 09:31 introduced a null dereference","confidence":0.8,"contributing_factors":[],"recommended_actions":["roll back"],"narrative":"n"}"#,
                ))
            }
        }
    }

    #[test]
    fn judge_body_uses_structured_output_and_fallback() {
        let scenarios = load_dir(&crate::default_golden_dir()).unwrap();
        let rca = ReasoningOutput {
            root_cause: "x".into(),
            confidence: 0.5,
            contributing_factors: vec![],
            recommended_actions: vec![],
            narrative: "n".into(),
            engine: "claude".into(),
        };
        let body = judge_request_body(DEFAULT_JUDGE_MODEL, &scenarios[0], &rca);
        assert_eq!(body["model"], DEFAULT_JUDGE_MODEL);
        assert_eq!(body["fallbacks"], "default");
        assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        assert!(body.get("thinking").is_none());
        let user = body["messages"][0]["content"].as_str().unwrap();
        assert!(user.contains("deployment_correlation"));
    }

    #[test]
    fn refusal_is_an_error_not_a_verdict() {
        let body =
            json!({"content": [], "stop_reason": "refusal", "stop_details": {"category": "cyber"}});
        assert!(parse_judge_response(&body).is_err());
    }

    #[tokio::test]
    async fn grade_all_runs_engine_then_judge_against_stub() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/messages"))
            .and(header("x-api-key", "test-key"))
            .respond_with(StubAnthropic)
            .mount(&server)
            .await;

        let mut scenarios = load_dir(&crate::default_golden_dir()).unwrap();
        scenarios.truncate(1);
        let mut evaluated = crate::evaluate(scenarios).await.unwrap();
        let engine = ClaudeReasoningEngine::new(
            "test-key".into(),
            "claude-opus-5-5".into(),
            4096,
            Some(server.uri()),
        );
        let judge = Judge::new(
            "test-key".into(),
            DEFAULT_JUDGE_MODEL.into(),
            Some(server.uri()),
        );
        grade_all(&mut evaluated, &engine, &judge).await;

        let grade = evaluated[0].score.rca.as_ref().unwrap();
        assert_eq!(grade.engine, "claude");
        assert_eq!(grade.verdict, "correct");
        assert!(grade.correct);
        assert!(grade.root_cause.contains("null dereference"));
    }
}
