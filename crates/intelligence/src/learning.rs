//! Learning loop (Phase 3) — closes the feedback cycle.
//!
//! After an investigation (and any remediation), the platform observes what
//! happened in a post-action window and feeds the result back:
//!
//! 1. **Classify** the outcome from observed signals (the decision tree in
//!    `docs/diagrams/phase3-architecture.md` → *Learning Loop*).
//! 2. **Embed** the incident — summary + findings + remediation + outcome — into
//!    the [`KnowledgeStore`] so future investigations retrieve it as grounding.
//! 3. **Update confidence** per finding/remediation category, building the
//!    success-rate signal the progressive-trust model (next unit) consumes.
//!
//! Best-effort and clock-free: callers stamp timestamps; this crate stays
//! deterministic so its tests need no wall clock.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::knowledge::{IncidentRecord, KnowledgeError, KnowledgeStore};

/// The verdict on a remediation/investigation after the monitoring window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// The alert cleared (or stayed clear) — the action worked.
    Success,
    /// The alert re-fired — the action did not hold.
    Failure,
    /// Nothing was acted on (read-only investigation), so there's no verdict.
    Inconclusive,
}

impl Outcome {
    pub fn as_str(&self) -> &'static str {
        match self {
            Outcome::Success => "success",
            Outcome::Failure => "failure",
            Outcome::Inconclusive => "inconclusive",
        }
    }
}

/// Signals observed after an investigation/remediation, fed to [`Outcome`]
/// classification. The orchestrator/monitor populates these; the classifier is
/// pure so the decision tree is unit-testable.
#[derive(Debug, Clone, Copy, Default)]
pub struct OutcomeSignals {
    /// Whether a remediation actually ran (false for read-only investigations).
    pub remediation_executed: bool,
    /// Alert cleared within the success window (diagram: 15 min).
    pub alert_resolved_within_window: bool,
    /// Same alert re-fired within the failure window (diagram: 1 hr).
    pub alert_refired_within_hour: bool,
}

impl OutcomeSignals {
    /// Classify per the Learning Loop decision tree:
    /// - no remediation ran → `Inconclusive` (nothing to judge);
    /// - resolved within the window → `Success`;
    /// - else re-fired within the hour → `Failure`;
    /// - else (quiet, never came back) → `Success`.
    pub fn classify(&self) -> Outcome {
        if !self.remediation_executed {
            return Outcome::Inconclusive;
        }
        if self.alert_resolved_within_window {
            return Outcome::Success;
        }
        if self.alert_refired_within_hour {
            return Outcome::Failure;
        }
        Outcome::Success
    }
}

/// Running success/failure tally for a category key.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CategoryStat {
    pub successes: u64,
    pub failures: u64,
}

impl CategoryStat {
    pub fn total(&self) -> u64 {
        self.successes + self.failures
    }

    /// Success rate in `[0, 1]`; 0 when there is no decisive history yet.
    pub fn success_rate(&self) -> f64 {
        if self.total() == 0 {
            0.0
        } else {
            self.successes as f64 / self.total() as f64
        }
    }
}

/// Per-category confidence tracker — the "update confidence scores" box. Keyed
/// by finding category (e.g. `"ErrorSpike"`) and remediation (e.g.
/// `"RestartWorkload"`). `Inconclusive` outcomes don't move the needle.
#[derive(Debug, Default)]
pub struct ConfidenceTracker {
    stats: HashMap<String, CategoryStat>,
}

impl ConfidenceTracker {
    pub fn record(&mut self, key: &str, outcome: Outcome) {
        let entry = self.stats.entry(key.to_string()).or_default();
        match outcome {
            Outcome::Success => entry.successes += 1,
            Outcome::Failure => entry.failures += 1,
            Outcome::Inconclusive => {}
        }
    }

    pub fn stat(&self, key: &str) -> CategoryStat {
        self.stats.get(key).copied().unwrap_or_default()
    }
}

/// Ties outcome tracking to the knowledge store: classify → embed → update
/// confidence. Held by the orchestrator; invoked best-effort at the tail of an
/// investigation and (later) after remediation verification.
pub struct LearningLoop {
    store: Arc<dyn KnowledgeStore>,
    tracker: Mutex<ConfidenceTracker>,
}

impl LearningLoop {
    pub fn new(store: Arc<dyn KnowledgeStore>) -> Self {
        Self {
            store,
            tracker: Mutex::new(ConfidenceTracker::default()),
        }
    }

    /// Classify the outcome, stamp it on the incident, update confidence stats,
    /// and embed the incident into the knowledge store. Returns the classified
    /// outcome. The knowledge write is surfaced as `Err` for logging but callers
    /// treat it as non-fatal.
    pub async fn record_outcome(
        &self,
        mut incident: IncidentRecord,
        signals: OutcomeSignals,
    ) -> Result<Outcome, KnowledgeError> {
        let outcome = signals.classify();
        incident.outcome = Some(outcome.as_str().to_string());

        // Update confidence stats — lock scoped, never held across the await.
        {
            let mut tracker = self.tracker.lock().expect("confidence tracker lock");
            for category in &incident.finding_categories {
                tracker.record(category, outcome);
            }
            if let Some(remediation) = &incident.remediation {
                tracker.record(remediation, outcome);
            }
        }

        self.store.record(&incident).await.map_err(|e| {
            warn!(error = %e, "Failed to embed incident — learning is best-effort");
            e
        })?;
        debug!(
            investigation_id = %incident.investigation_id,
            outcome = outcome.as_str(),
            "Incident embedded into knowledge store"
        );
        Ok(outcome)
    }

    /// Current success rate for a category key (for the progressive-trust model).
    pub fn success_rate(&self, key: &str) -> f64 {
        self.tracker
            .lock()
            .expect("confidence tracker lock")
            .stat(key)
            .success_rate()
    }

    /// Snapshot a category's tally.
    pub fn stat(&self, key: &str) -> CategoryStat {
        self.tracker
            .lock()
            .expect("confidence tracker lock")
            .stat(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::hash_embedder;
    use crate::knowledge::MockKnowledgeStore;

    fn incident() -> IncidentRecord {
        IncidentRecord {
            investigation_id: "1".to_string(),
            affected_resource: "payments-api".to_string(),
            trigger: "High Error Rate".to_string(),
            severity: "Sev1".to_string(),
            root_cause: "deploy regression".to_string(),
            summary: "500s after deploy".to_string(),
            finding_categories: vec!["ErrorSpike".to_string()],
            remediation: Some("RestartWorkload".to_string()),
            outcome: None,
            recorded_at: None,
        }
    }

    #[test]
    fn classify_no_remediation_is_inconclusive() {
        let s = OutcomeSignals {
            remediation_executed: false,
            alert_resolved_within_window: true,
            alert_refired_within_hour: false,
        };
        assert_eq!(s.classify(), Outcome::Inconclusive);
    }

    #[test]
    fn classify_resolved_is_success() {
        let s = OutcomeSignals {
            remediation_executed: true,
            alert_resolved_within_window: true,
            alert_refired_within_hour: false,
        };
        assert_eq!(s.classify(), Outcome::Success);
    }

    #[test]
    fn classify_refired_is_failure() {
        let s = OutcomeSignals {
            remediation_executed: true,
            alert_resolved_within_window: false,
            alert_refired_within_hour: true,
        };
        assert_eq!(s.classify(), Outcome::Failure);
    }

    #[test]
    fn classify_quiet_is_success() {
        let s = OutcomeSignals {
            remediation_executed: true,
            alert_resolved_within_window: false,
            alert_refired_within_hour: false,
        };
        assert_eq!(s.classify(), Outcome::Success);
    }

    #[tokio::test]
    async fn record_outcome_embeds_and_updates_confidence() {
        let store = Arc::new(MockKnowledgeStore::new(hash_embedder(256)));
        let loop_ = LearningLoop::new(store.clone());

        let outcome = loop_
            .record_outcome(
                incident(),
                OutcomeSignals {
                    remediation_executed: true,
                    alert_resolved_within_window: true,
                    alert_refired_within_hour: false,
                },
            )
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Success);
        assert_eq!(store.len(), 1);
        assert_eq!(loop_.success_rate("ErrorSpike"), 1.0);
        assert_eq!(loop_.success_rate("RestartWorkload"), 1.0);
    }

    #[tokio::test]
    async fn failure_then_success_yields_half_rate() {
        let store = Arc::new(MockKnowledgeStore::new(hash_embedder(256)));
        let loop_ = LearningLoop::new(store);

        loop_
            .record_outcome(
                incident(),
                OutcomeSignals {
                    remediation_executed: true,
                    alert_resolved_within_window: false,
                    alert_refired_within_hour: true,
                },
            )
            .await
            .unwrap();
        loop_
            .record_outcome(
                incident(),
                OutcomeSignals {
                    remediation_executed: true,
                    alert_resolved_within_window: true,
                    alert_refired_within_hour: false,
                },
            )
            .await
            .unwrap();

        let stat = loop_.stat("ErrorSpike");
        assert_eq!(stat.successes, 1);
        assert_eq!(stat.failures, 1);
        assert_eq!(stat.success_rate(), 0.5);
    }

    #[tokio::test]
    async fn inconclusive_does_not_move_confidence() {
        let store = Arc::new(MockKnowledgeStore::new(hash_embedder(256)));
        let loop_ = LearningLoop::new(store.clone());

        let outcome = loop_
            .record_outcome(incident(), OutcomeSignals::default())
            .await
            .unwrap();

        assert_eq!(outcome, Outcome::Inconclusive);
        // Still embedded (read-only investigations are retrievable)…
        assert_eq!(store.len(), 1);
        // …but the confidence tally is untouched.
        assert_eq!(loop_.stat("ErrorSpike"), CategoryStat::default());
    }
}
