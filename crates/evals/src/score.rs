//! Order-independent scoring of a scenario run against its labels.
//!
//! Agents reply asynchronously, so findings that tie on severity arrive in a
//! nondeterministic order. No metric here depends on that order: a tie at the
//! top is scored as a miss and reported as ambiguous, never as a coin flip.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use sre_common::findings::{Finding, FindingCategory, FindingSeverity};

use crate::runner::{ProposedAction, RunOutcome};
use crate::scenario::{category_name, Fixture, Label, Scenario, NONE_LABEL};

/// Verdict recorded when the reasoning engine or the judge call failed.
pub const RCA_ERROR_VERDICT: &str = "error";

/// Per-scenario checks recorded in the CI ratchet baseline.
pub const CHECKS: &[&str] = &["root_cause", "signals", "escalation", "remediation"];

/// A call the pipeline made, or failed to make unambiguously.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Call {
    /// Exactly one answer.
    One(String),
    /// No answer (no qualifying finding, or no proposal).
    Nothing,
    /// Several answers tie at the top; which one a reader sees first depends
    /// on agent arrival order.
    Tied(Vec<String>),
}

impl Call {
    pub fn display(&self) -> String {
        match self {
            Call::One(v) => v.clone(),
            Call::Nothing => NONE_LABEL.to_string(),
            Call::Tied(vs) => format!("tie: {}", vs.join(" / ")),
        }
    }
}

/// A finding as it appears in the scorecard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FindingRow {
    pub category: String,
    pub severity: String,
    pub confidence: f64,
    pub agent: String,
    pub title: String,
}

/// Grade of an LLM-written root-cause analysis (optional tier).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RcaGrade {
    pub engine: String,
    pub root_cause: String,
    pub verdict: String,
    pub correct: bool,
    pub judge_reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScenarioScore {
    pub id: String,
    pub title: String,
    pub tags: Vec<String>,
    pub expected_root_cause: String,
    pub predicted_root_cause: Call,
    pub root_cause_correct: bool,
    /// The labeled root cause appears among the detections at any rank.
    /// `None` for scenarios labeled `none`.
    pub root_cause_detected: Option<bool>,
    /// Categories counted as detections (see [`is_detection`]).
    pub detected: Vec<String>,
    pub signal_tp: Vec<String>,
    pub signal_fp: Vec<String>,
    pub signal_fn: Vec<String>,
    pub expected_escalate: bool,
    pub escalated: bool,
    pub expected_remediation: Vec<String>,
    pub first_action: Call,
    /// Every proposal, most confident first.
    pub proposals: Vec<ProposedAction>,
    pub remediation_correct: bool,
    /// An action was proposed where the label says no automated action fits.
    pub unwarranted_action: bool,
    pub findings: Vec<FindingRow>,
    /// Fixture telemetry the agents never queried.
    pub unqueried_fixtures: Vec<String>,
    /// Requests whose KQL matched no known template (fixture routing broke).
    pub unrecognized_queries: usize,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rca: Option<RcaGrade>,
}

impl ScenarioScore {
    pub fn signals_correct(&self) -> bool {
        self.signal_fp.is_empty() && self.signal_fn.is_empty()
    }

    pub fn escalation_correct(&self) -> bool {
        self.expected_escalate == self.escalated
    }

    pub fn check(&self, name: &str) -> bool {
        match name {
            "root_cause" => self.root_cause_correct,
            "signals" => self.signals_correct(),
            "escalation" => self.escalation_correct(),
            "remediation" => self.remediation_correct,
            _ => false,
        }
    }

    pub fn all_checks_pass(&self) -> bool {
        CHECKS.iter().all(|c| self.check(c))
    }
}

/// A finding counts as a detection, and as a root-cause claim, when it is not
/// informational and is Medium severity or above. Low and Info findings are
/// status readouts (for example the latency summary emitted on every run).
pub fn is_detection(f: &Finding) -> bool {
    f.category != FindingCategory::Informational && f.severity <= FindingSeverity::Medium
}

/// The root cause the report leads with: the categories in the most severe
/// tier of detections.
pub fn root_cause_call(findings: &[Finding]) -> Call {
    let detections: Vec<&Finding> = findings.iter().filter(|f| is_detection(f)).collect();
    let Some(top) = detections.iter().map(|f| f.severity).min() else {
        return Call::Nothing;
    };
    let tier: BTreeSet<String> = detections
        .iter()
        .filter(|f| f.severity == top)
        .map(|f| category_name(&f.category))
        .collect();
    let mut tier: Vec<String> = tier.into_iter().collect();
    if tier.len() == 1 {
        Call::One(tier.remove(0))
    } else {
        Call::Tied(tier)
    }
}

/// The remediation a reader sees first: the most confident proposal.
pub fn first_action_call(outcome: &RunOutcome) -> Call {
    let Some(top) = outcome.proposals.first() else {
        return Call::Nothing;
    };
    let tied: BTreeSet<String> = outcome
        .proposals
        .iter()
        .filter(|p| (p.confidence - top.confidence).abs() < 1e-9)
        .map(|p| p.action.clone())
        .collect();
    let mut tied: Vec<String> = tied.into_iter().collect();
    if tied.len() == 1 {
        Call::One(tied.remove(0))
    } else {
        Call::Tied(tied)
    }
}

pub fn score(scenario: &Scenario, outcome: &RunOutcome) -> ScenarioScore {
    let report = &outcome.report;
    let root_label = Label::parse(&scenario.expected.root_cause);
    let predicted_root_cause = root_cause_call(&report.findings);
    let root_cause_correct = match &predicted_root_cause {
        Call::One(c) => root_label.matches(c),
        Call::Nothing => root_label.matches(NONE_LABEL),
        Call::Tied(_) => false,
    };

    let detected: Vec<String> = report
        .findings
        .iter()
        .filter(|f| is_detection(f))
        .map(|f| category_name(&f.category))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();

    let root_cause_detected = if root_label.matches(NONE_LABEL) {
        None
    } else {
        Some(detected.iter().any(|d| root_label.matches(d)))
    };

    let signal_labels: Vec<Label> = scenario
        .expected
        .signals
        .iter()
        .map(|s| Label::parse(s))
        .collect();
    let mut signal_tp = Vec::new();
    let mut signal_fn = Vec::new();
    for label in &signal_labels {
        // Credit the category the pipeline actually emitted, not the label's
        // canonical name, so the per-category table never shows a detection
        // for a category no agent produces.
        if let Some(hit) = detected.iter().find(|d| label.matches(d)) {
            signal_tp.push(hit.clone());
        } else {
            signal_fn.push(label.canonical().to_string());
        }
    }
    let signal_fp: Vec<String> = detected
        .iter()
        .filter(|d| !signal_labels.iter().any(|l| l.matches(d)))
        .cloned()
        .collect();

    let first_action = first_action_call(outcome);
    let accepted = &scenario.expected.remediation;
    let remediation_correct = match &first_action {
        Call::One(a) => accepted.contains(a),
        Call::Nothing => accepted.iter().any(|a| a == NONE_LABEL),
        Call::Tied(_) => false,
    };
    let none_only = accepted.iter().all(|a| a == NONE_LABEL);
    let unwarranted_action = none_only && first_action != Call::Nothing;

    let queried: BTreeSet<&str> = outcome.queries.iter().filter_map(|q| q.template).collect();
    let unqueried_fixtures: Vec<String> = scenario
        .telemetry
        .iter()
        .filter(|(key, fixture)| {
            let has_data = match fixture {
                Fixture::Rows(rows) => !rows.is_empty(),
                Fixture::Error { .. } => true,
            };
            key.as_str() != crate::fixture::WILDCARD_KEY
                && has_data
                && !queried.contains(key.as_str())
        })
        .map(|(key, _)| key.clone())
        .collect();

    ScenarioScore {
        id: scenario.id.clone(),
        title: scenario.title.clone(),
        tags: scenario.tags.clone(),
        expected_root_cause: scenario.expected.root_cause.clone(),
        predicted_root_cause,
        root_cause_correct,
        root_cause_detected,
        detected,
        signal_tp,
        signal_fp,
        signal_fn,
        expected_escalate: scenario.expected.escalate,
        escalated: report.escalated,
        expected_remediation: accepted.clone(),
        first_action,
        proposals: outcome.proposals.clone(),
        remediation_correct,
        unwarranted_action,
        findings: report
            .findings
            .iter()
            .map(|f| FindingRow {
                category: category_name(&f.category),
                severity: f.severity.to_string(),
                confidence: f.confidence,
                agent: f.source_agent.clone(),
                title: f.title.clone(),
            })
            .collect(),
        unqueried_fixtures,
        unrecognized_queries: outcome
            .queries
            .iter()
            .filter(|q| q.template.is_none())
            .count(),
        duration_ms: report.duration_ms,
        rca: None,
    }
}

/// A count out of a denominator.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Ratio {
    pub hits: usize,
    pub total: usize,
}

impl Ratio {
    pub fn pct(&self) -> Option<f64> {
        (self.total > 0).then(|| 100.0 * self.hits as f64 / self.total as f64)
    }

    pub fn display(&self) -> String {
        match self.pct() {
            Some(p) => format!("{p:.0}% ({} of {})", self.hits, self.total),
            None => "n/a (0 of 0)".to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CategoryStats {
    pub tp: usize,
    pub fp: usize,
    #[serde(rename = "fn")]
    pub fn_: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Summary {
    pub scenarios: usize,
    pub root_cause_top1: Ratio,
    pub root_cause_ambiguous: usize,
    pub root_cause_detected: Ratio,
    pub detection_precision: Ratio,
    pub detection_recall: Ratio,
    pub per_category: BTreeMap<String, CategoryStats>,
    pub escalation: Ratio,
    pub missed_pages: usize,
    pub unnecessary_pages: usize,
    pub remediation: Ratio,
    pub unwarranted_actions: usize,
    pub all_checks: Ratio,
    pub mean_duration_ms: f64,
    /// LLM RCAs judged correct, out of those that were graded. API or parse
    /// failures are excluded from the denominator and counted in `rca_errors`,
    /// so an overloaded endpoint never reads as a wrong analysis.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rca_correct: Option<Ratio>,
    #[serde(default)]
    pub rca_errors: usize,
}

pub fn summarize(scores: &[ScenarioScore]) -> Summary {
    let n = scores.len();
    let count = |pred: &dyn Fn(&ScenarioScore) -> bool| scores.iter().filter(|s| pred(s)).count();

    let mut per_category: BTreeMap<String, CategoryStats> = BTreeMap::new();
    for s in scores {
        for c in &s.signal_tp {
            per_category.entry(c.clone()).or_default().tp += 1;
        }
        for c in &s.signal_fp {
            per_category.entry(c.clone()).or_default().fp += 1;
        }
        for c in &s.signal_fn {
            per_category.entry(c.clone()).or_default().fn_ += 1;
        }
    }
    let tp: usize = per_category.values().map(|c| c.tp).sum();
    let fp: usize = per_category.values().map(|c| c.fp).sum();
    let fn_: usize = per_category.values().map(|c| c.fn_).sum();

    let with_root: Vec<&ScenarioScore> = scores
        .iter()
        .filter(|s| s.root_cause_detected.is_some())
        .collect();
    let attempted: Vec<&RcaGrade> = scores.iter().filter_map(|s| s.rca.as_ref()).collect();
    let graded: Vec<&&RcaGrade> = attempted
        .iter()
        .filter(|g| g.verdict != RCA_ERROR_VERDICT)
        .collect();

    Summary {
        scenarios: n,
        root_cause_top1: Ratio {
            hits: count(&|s| s.root_cause_correct),
            total: n,
        },
        root_cause_ambiguous: count(&|s| matches!(s.predicted_root_cause, Call::Tied(_))),
        root_cause_detected: Ratio {
            hits: with_root
                .iter()
                .filter(|s| s.root_cause_detected == Some(true))
                .count(),
            total: with_root.len(),
        },
        detection_precision: Ratio {
            hits: tp,
            total: tp + fp,
        },
        detection_recall: Ratio {
            hits: tp,
            total: tp + fn_,
        },
        per_category,
        escalation: Ratio {
            hits: count(&|s| s.escalation_correct()),
            total: n,
        },
        missed_pages: count(&|s| s.expected_escalate && !s.escalated),
        unnecessary_pages: count(&|s| !s.expected_escalate && s.escalated),
        remediation: Ratio {
            hits: count(&|s| s.remediation_correct),
            total: n,
        },
        unwarranted_actions: count(&|s| s.unwarranted_action),
        all_checks: Ratio {
            hits: count(&|s| s.all_checks_pass()),
            total: n,
        },
        mean_duration_ms: if n == 0 {
            0.0
        } else {
            scores.iter().map(|s| s.duration_ms as f64).sum::<f64>() / n as f64
        },
        rca_correct: (!attempted.is_empty()).then(|| Ratio {
            hits: graded.iter().filter(|g| g.correct).count(),
            total: graded.len(),
        }),
        rca_errors: attempted.len() - graded.len(),
    }
}

/// Per-scenario pass/fail for each check, as stored in `baseline.json`.
pub type Baseline = BTreeMap<String, BTreeMap<String, bool>>;

pub fn to_baseline(scores: &[ScenarioScore]) -> Baseline {
    scores
        .iter()
        .map(|s| {
            let checks = CHECKS.iter().map(|c| (c.to_string(), s.check(c))).collect();
            (s.id.clone(), checks)
        })
        .collect()
}

/// Ratchet comparison: checks that passed in the baseline and fail now, and
/// checks that newly pass.
pub fn compare(baseline: &Baseline, scores: &[ScenarioScore]) -> (Vec<String>, Vec<String>) {
    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    for (id, checks) in baseline {
        let Some(score) = scores.iter().find(|s| &s.id == id) else {
            regressions.push(format!("{id}: scenario missing from the golden set"));
            continue;
        };
        for (check, was_passing) in checks {
            let now = score.check(check);
            if *was_passing && !now {
                regressions.push(format!("{id}: {check} regressed (pass -> fail)"));
            } else if !*was_passing && now {
                improvements.push(format!("{id}: {check} now passes"));
            }
        }
    }
    for score in scores {
        if !baseline.contains_key(&score.id) {
            improvements.push(format!("{}: new scenario, not yet in baseline", score.id));
        }
    }
    (regressions, improvements)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finding(category: FindingCategory, severity: FindingSeverity) -> Finding {
        Finding {
            category,
            severity,
            title: "t".to_string(),
            description: String::new(),
            confidence: 0.8,
            observed_at: None,
            source_agent: "a".to_string(),
            evidence: vec![],
            suggestions: vec![],
        }
    }

    #[test]
    fn root_cause_is_the_single_category_in_the_top_tier() {
        let f = vec![
            finding(
                FindingCategory::DeploymentCorrelation,
                FindingSeverity::Medium,
            ),
            finding(FindingCategory::ErrorSpike, FindingSeverity::Critical),
            finding(FindingCategory::ErrorSpike, FindingSeverity::Critical),
        ];
        assert_eq!(root_cause_call(&f), Call::One("error_spike".to_string()));
    }

    #[test]
    fn top_tier_tie_is_reported_not_guessed() {
        let f = vec![
            finding(FindingCategory::ServiceHealth, FindingSeverity::Critical),
            finding(FindingCategory::ErrorSpike, FindingSeverity::Critical),
        ];
        assert!(matches!(root_cause_call(&f), Call::Tied(v) if v.len() == 2));
    }

    #[test]
    fn low_and_informational_findings_are_not_claims() {
        let f = vec![
            finding(FindingCategory::LatencyDegradation, FindingSeverity::Low),
            finding(FindingCategory::Informational, FindingSeverity::Critical),
            finding(FindingCategory::ErrorSpike, FindingSeverity::Info),
        ];
        assert_eq!(root_cause_call(&f), Call::Nothing);
    }

    #[test]
    fn ratio_display_handles_empty_denominator() {
        assert_eq!(Ratio { hits: 0, total: 0 }.display(), "n/a (0 of 0)");
        assert_eq!(Ratio { hits: 1, total: 4 }.display(), "25% (1 of 4)");
    }
}
