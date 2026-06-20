use async_trait::async_trait;

use sre_common::findings::FindingCategory;

use crate::reasoning::{ReasoningEngine, ReasoningError, ReasoningInput, ReasoningOutput};

/// Deterministic, offline reasoning engine. Synthesizes an RCA from the
/// findings using simple rules — no LLM, no network, reproducible. Used in CI,
/// in dev without an API key, and as the fallback when the Claude engine is
/// unavailable.
#[derive(Clone, Default)]
pub struct MockReasoningEngine;

impl MockReasoningEngine {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ReasoningEngine for MockReasoningEngine {
    fn name(&self) -> &str {
        "mock"
    }

    async fn synthesize(&self, input: &ReasoningInput) -> Result<ReasoningOutput, ReasoningError> {
        Ok(synthesize_deterministic(input))
    }
}

/// Rule-based synthesis (pure, testable). The most severe finding drives the
/// root cause; suggestions across findings become recommended actions.
pub fn synthesize_deterministic(input: &ReasoningInput) -> ReasoningOutput {
    // FindingSeverity derives Ord in declaration order (Critical < … < Info),
    // so the minimum is the most severe.
    let most_severe = input.findings.iter().min_by_key(|f| f.severity);

    let root_cause = match most_severe {
        Some(f) => format!("{} — {}", category_phrase(&f.category), f.title),
        None => "No significant signals found — likely transient or below detection thresholds"
            .to_string(),
    };

    let contributing_factors: Vec<String> = {
        let mut sorted: Vec<_> = input.findings.iter().collect();
        sorted.sort_by_key(|f| f.severity);
        sorted
            .iter()
            .take(5)
            .map(|f| format!("[{}] {}", f.severity, f.title))
            .collect()
    };

    let recommended_actions: Vec<String> = {
        let mut acts: Vec<String> = Vec::new();
        for f in &input.findings {
            for s in &f.suggestions {
                if !acts.contains(s) {
                    acts.push(s.clone());
                }
            }
        }
        acts.truncate(5);
        if acts.is_empty() {
            acts.push("Escalate to on-call for manual review".to_string());
        }
        acts
    };

    let related = if input.related_incidents.is_empty() {
        String::new()
    } else {
        format!(
            " {} related past incident(s) were retrieved for context.",
            input.related_incidents.len()
        )
    };

    let narrative = format!(
        "Investigation of {} ({}) surfaced {} finding(s). {}{} Most likely root cause: {}.",
        input.affected_resource,
        input.severity,
        input.findings.len(),
        input.agent_summary,
        related,
        root_cause,
    );

    let confidence = if input.findings.is_empty() {
        0.3
    } else {
        input.overall_confidence.max(0.5)
    };

    ReasoningOutput {
        root_cause,
        confidence,
        contributing_factors,
        recommended_actions,
        narrative,
        engine: "mock".to_string(),
    }
}

fn category_phrase(category: &FindingCategory) -> &'static str {
    match category {
        FindingCategory::ErrorSpike => "Error/exception spike",
        FindingCategory::LatencyDegradation => "Latency degradation",
        FindingCategory::ResourceExhaustion => "Resource exhaustion",
        FindingCategory::DependencyFailure => "Downstream dependency failure",
        FindingCategory::DeploymentCorrelation => "Recent deployment / change",
        FindingCategory::TrafficAnomaly => "Traffic anomaly",
        FindingCategory::ConfigChange => "Configuration change",
        FindingCategory::ServiceHealth => "Resource/service health degradation",
        FindingCategory::Informational => "Informational signal",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::findings::{Finding, FindingSeverity};

    fn finding(category: FindingCategory, severity: FindingSeverity, title: &str) -> Finding {
        Finding {
            category,
            severity,
            title: title.to_string(),
            description: "d".to_string(),
            confidence: 0.9,
            observed_at: None,
            source_agent: "logs-agent".to_string(),
            evidence: vec![],
            suggestions: vec!["restart the workload".to_string()],
        }
    }

    fn input(findings: Vec<Finding>) -> ReasoningInput {
        ReasoningInput {
            investigation_id: "id".to_string(),
            affected_resource: "payments-api".to_string(),
            trigger: "High Error Rate".to_string(),
            severity: "Sev1".to_string(),
            findings,
            agent_summary: "Agents found error spikes.".to_string(),
            overall_confidence: 0.8,
            related_incidents: vec![],
        }
    }

    #[tokio::test]
    async fn synthesizes_from_most_severe_finding() {
        let engine = MockReasoningEngine::new();
        let out = engine
            .synthesize(&input(vec![
                finding(
                    FindingCategory::LatencyDegradation,
                    FindingSeverity::Medium,
                    "slow",
                ),
                finding(
                    FindingCategory::ErrorSpike,
                    FindingSeverity::Critical,
                    "500s",
                ),
            ]))
            .await
            .unwrap();
        assert_eq!(out.engine, "mock");
        assert!(out.root_cause.contains("Error/exception spike"));
        assert!(out.root_cause.contains("500s"));
        assert!(!out.recommended_actions.is_empty());
        assert!(out.confidence >= 0.5);
    }

    #[tokio::test]
    async fn no_findings_yields_low_confidence_escalation() {
        let engine = MockReasoningEngine::new();
        let out = engine.synthesize(&input(vec![])).await.unwrap();
        assert!(out.confidence < 0.5);
        assert_eq!(
            out.recommended_actions,
            vec!["Escalate to on-call for manual review"]
        );
    }

    #[tokio::test]
    async fn dedups_recommended_actions() {
        let engine = MockReasoningEngine::new();
        let out = engine
            .synthesize(&input(vec![
                finding(FindingCategory::ErrorSpike, FindingSeverity::High, "a"),
                finding(FindingCategory::ErrorSpike, FindingSeverity::High, "b"),
            ]))
            .await
            .unwrap();
        // Both findings carry the same "restart the workload" suggestion.
        assert_eq!(out.recommended_actions.len(), 1);
    }
}
