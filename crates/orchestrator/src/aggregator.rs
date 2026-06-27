use sre_common::findings::{Finding, FindingSeverity};

/// Confidence threshold below which we escalate to humans
const ESCALATION_CONFIDENCE_THRESHOLD: f64 = 0.5;

/// Aggregates findings from multiple agents and scores overall confidence.
pub struct FindingAggregator;

impl FindingAggregator {
    /// Calculate overall confidence from a set of findings
    pub fn overall_confidence(findings: &[Finding]) -> f64 {
        if findings.is_empty() {
            return 0.0;
        }

        // Weight by severity: critical findings contribute more to confidence
        let weighted_sum: f64 = findings
            .iter()
            .map(|f| {
                let weight = match f.severity {
                    FindingSeverity::Critical => 2.0,
                    FindingSeverity::High => 1.5,
                    FindingSeverity::Medium => 1.0,
                    FindingSeverity::Low => 0.5,
                    FindingSeverity::Info => 0.25,
                };
                f.confidence * weight
            })
            .sum();

        let total_weight: f64 = findings
            .iter()
            .map(|f| match f.severity {
                FindingSeverity::Critical => 2.0,
                FindingSeverity::High => 1.5,
                FindingSeverity::Medium => 1.0,
                FindingSeverity::Low => 0.5,
                FindingSeverity::Info => 0.25,
            })
            .sum();

        (weighted_sum / total_weight).clamp(0.0, 1.0)
    }

    /// Determine if we should escalate based on findings
    pub fn should_escalate(findings: &[Finding]) -> bool {
        if findings.is_empty() {
            return true; // No findings at all — escalate
        }

        let confidence = Self::overall_confidence(findings);
        confidence < ESCALATION_CONFIDENCE_THRESHOLD
    }

    /// Deduplicate findings from multiple agents that describe the same issue
    pub fn deduplicate(findings: Vec<Finding>) -> Vec<Finding> {
        let mut deduped: Vec<Finding> = Vec::new();

        for finding in findings {
            let is_dup = deduped.iter().any(|existing| {
                existing.category == finding.category
                    && existing.title == finding.title
                    && existing.source_agent != finding.source_agent
            });

            if !is_dup {
                deduped.push(finding);
            }
        }

        // Sort by severity (most severe first)
        deduped.sort_by_key(|a| a.severity);
        deduped
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::findings::{FindingCategory, FindingSeverity};

    fn make_finding(severity: FindingSeverity, confidence: f64) -> Finding {
        Finding {
            category: FindingCategory::ErrorSpike,
            severity,
            title: "test".to_string(),
            description: "test".to_string(),
            confidence,
            observed_at: None,
            source_agent: "test".to_string(),
            evidence: vec![],
            suggestions: vec![],
        }
    }

    #[test]
    fn test_overall_confidence_empty() {
        assert_eq!(FindingAggregator::overall_confidence(&[]), 0.0);
    }

    #[test]
    fn test_overall_confidence_single_critical() {
        let findings = vec![make_finding(FindingSeverity::Critical, 0.9)];
        let conf = FindingAggregator::overall_confidence(&findings);
        assert!((conf - 0.9).abs() < 0.01);
    }

    #[test]
    fn test_should_escalate_no_findings() {
        assert!(FindingAggregator::should_escalate(&[]));
    }

    #[test]
    fn test_should_not_escalate_high_confidence() {
        let findings = vec![make_finding(FindingSeverity::High, 0.9)];
        assert!(!FindingAggregator::should_escalate(&findings));
    }

    #[test]
    fn deduplicate_drops_cross_agent_duplicates_and_sorts_by_severity() {
        let mut a = make_finding(FindingSeverity::Low, 0.5);
        a.title = "Error spike".to_string();
        a.source_agent = "logs-agent".to_string();

        // Same category + title but a *different* agent => duplicate of `a`.
        let mut b = make_finding(FindingSeverity::Critical, 0.9);
        b.title = "Error spike".to_string();
        b.source_agent = "metrics-agent".to_string();

        // Distinct title => kept.
        let mut c = make_finding(FindingSeverity::High, 0.8);
        c.title = "Latency degradation".to_string();
        c.source_agent = "metrics-agent".to_string();

        let deduped = FindingAggregator::deduplicate(vec![a, b, c]);
        assert_eq!(deduped.len(), 2);
        // Sorted most-severe first: High "Latency degradation" precedes Low "Error spike".
        assert_eq!(deduped[0].title, "Latency degradation");
        assert_eq!(deduped[1].title, "Error spike");
    }

    #[test]
    fn deduplicate_keeps_same_agent_repeats() {
        // The dedup rule only collapses findings from *different* agents, so two
        // identical findings from the same agent are both retained.
        let mut a = make_finding(FindingSeverity::Medium, 0.7);
        a.title = "Same".to_string();
        a.source_agent = "logs-agent".to_string();
        let b = a.clone();

        let deduped = FindingAggregator::deduplicate(vec![a, b]);
        assert_eq!(deduped.len(), 2);
    }
}
