use std::sync::Arc;

use futures_util::future::join_all;
use tracing::{info, warn};

use sre_common::config::NotificationConfig;
use sre_common::findings::{FindingSeverity, InvestigationReport};

use crate::notification::{report_severity, Notification};
use crate::providers::{MockNotifier, PagerDutyNotifier, SlackNotifier, TeamsNotifier};
use crate::traits::Notifier;

/// Decides whether a report warrants a notification and fans out to every
/// configured provider concurrently (best-effort).
pub struct NotificationEngine {
    notifiers: Vec<Arc<dyn Notifier>>,
    /// Notify only when the report's severity is at least this severe.
    min_severity: FindingSeverity,
    /// Always notify on escalation regardless of severity.
    notify_on_escalation: bool,
}

impl NotificationEngine {
    /// Build the engine from config. Returns `None` when notifications are
    /// disabled. When enabled but no real provider is configured, a logging
    /// [`MockNotifier`] is installed so the path still runs in dev.
    pub fn from_config(cfg: &NotificationConfig) -> Option<Self> {
        if !cfg.enabled {
            return None;
        }

        let mut notifiers: Vec<Arc<dyn Notifier>> = Vec::new();
        if let Some(slack) = &cfg.slack {
            notifiers.push(Arc::new(SlackNotifier::new(slack.webhook_url.clone())));
        }
        if let Some(teams) = &cfg.teams {
            notifiers.push(Arc::new(TeamsNotifier::new(teams.webhook_url.clone())));
        }
        if let Some(pd) = &cfg.pagerduty {
            notifiers.push(Arc::new(PagerDutyNotifier::new(pd.routing_key.clone())));
        }
        if notifiers.is_empty() {
            warn!("notifications enabled but no provider configured — using logging mock notifier");
            notifiers.push(Arc::new(MockNotifier::new()));
        }

        info!(
            providers = notifiers.len(),
            min_severity = %cfg.min_severity,
            notify_on_escalation = cfg.notify_on_escalation,
            "Notification engine configured"
        );

        Some(Self {
            notifiers,
            min_severity: parse_severity(&cfg.min_severity),
            notify_on_escalation: cfg.notify_on_escalation,
        })
    }

    /// Explicit constructor (tests / custom wiring).
    pub fn with_notifiers(
        notifiers: Vec<Arc<dyn Notifier>>,
        min_severity: FindingSeverity,
        notify_on_escalation: bool,
    ) -> Self {
        Self {
            notifiers,
            min_severity,
            notify_on_escalation,
        }
    }

    /// Whether a report warrants a notification under the configured policy.
    pub fn should_notify(&self, report: &InvestigationReport) -> bool {
        if report.escalated && self.notify_on_escalation {
            return true;
        }
        // Lower ordinal = more severe, so "at least min_severity" is
        // `severity <= min_severity`.
        report_severity(report) <= self.min_severity
    }

    /// Decide-then-dispatch. Returns the number of successful deliveries
    /// (0 when the report doesn't meet the policy).
    pub async fn notify_report(&self, report: &InvestigationReport) -> usize {
        if !self.should_notify(report) {
            return 0;
        }
        let notification = Notification::from_report(report);
        self.dispatch(&notification).await
    }

    /// Fan out a notification to every provider concurrently. Provider
    /// failures are logged, never propagated. Returns successful deliveries.
    pub async fn dispatch(&self, notification: &Notification) -> usize {
        let sends = self.notifiers.iter().map(|notifier| {
            let notifier = notifier.clone();
            async move {
                match notifier.send(notification).await {
                    Ok(()) => {
                        info!(
                            provider = notifier.name(),
                            investigation_id = %notification.investigation_id,
                            "notification delivered"
                        );
                        true
                    }
                    Err(e) => {
                        warn!(
                            provider = notifier.name(),
                            error = %e,
                            investigation_id = %notification.investigation_id,
                            "notification delivery failed"
                        );
                        false
                    }
                }
            }
        });
        join_all(sends).await.into_iter().filter(|ok| *ok).count()
    }
}

fn parse_severity(s: &str) -> FindingSeverity {
    match s.to_ascii_lowercase().as_str() {
        "critical" => FindingSeverity::Critical,
        "high" => FindingSeverity::High,
        "medium" => FindingSeverity::Medium,
        "low" => FindingSeverity::Low,
        _ => FindingSeverity::Info,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MockNotifier;
    use chrono::Utc;
    use sre_common::config::{NotificationConfig, PagerDutyConfig, SlackConfig, TeamsConfig};
    use sre_common::findings::{Finding, FindingCategory};

    fn finding(severity: FindingSeverity) -> Finding {
        Finding {
            category: FindingCategory::ErrorSpike,
            severity,
            title: "test finding".to_string(),
            description: "desc".to_string(),
            confidence: 0.9,
            observed_at: None,
            source_agent: "logs-agent".to_string(),
            evidence: vec![],
            suggestions: vec![],
        }
    }

    fn report(findings: Vec<Finding>, escalated: bool) -> InvestigationReport {
        let now = Utc::now();
        InvestigationReport {
            investigation_id: "00000000-0000-0000-0000-000000000001".to_string(),
            started_at: now,
            completed_at: now,
            duration_ms: 10,
            trigger: "alert-1".to_string(),
            affected_resource: "payments-api".to_string(),
            overall_confidence: 0.8,
            findings,
            summary: "summary".to_string(),
            agents_invoked: vec!["logs-agent".to_string()],
            escalated,
            escalation_reason: None,
        }
    }

    fn engine_with_mock(
        min: FindingSeverity,
        on_escalation: bool,
    ) -> (NotificationEngine, Arc<MockNotifier>) {
        let mock = Arc::new(MockNotifier::new());
        let engine = NotificationEngine::with_notifiers(vec![mock.clone()], min, on_escalation);
        (engine, mock)
    }

    #[test]
    fn high_threshold_notifies_on_critical_and_high_only() {
        let (engine, _) = engine_with_mock(FindingSeverity::High, false);
        assert!(engine.should_notify(&report(vec![finding(FindingSeverity::Critical)], false)));
        assert!(engine.should_notify(&report(vec![finding(FindingSeverity::High)], false)));
        assert!(!engine.should_notify(&report(vec![finding(FindingSeverity::Medium)], false)));
        assert!(!engine.should_notify(&report(vec![finding(FindingSeverity::Low)], false)));
    }

    #[test]
    fn escalation_notifies_regardless_of_severity() {
        let (engine, _) = engine_with_mock(FindingSeverity::High, true);
        // Low severity, but escalated => notify.
        assert!(engine.should_notify(&report(vec![finding(FindingSeverity::Low)], true)));
        // No findings, but escalated => notify.
        assert!(engine.should_notify(&report(vec![], true)));
    }

    #[tokio::test]
    async fn dispatch_records_on_mock_and_counts_deliveries() {
        let (engine, mock) = engine_with_mock(FindingSeverity::High, true);
        let delivered = engine
            .notify_report(&report(vec![finding(FindingSeverity::Critical)], false))
            .await;
        assert_eq!(delivered, 1);
        assert_eq!(mock.count(), 1);
        assert_eq!(mock.sent()[0].severity, FindingSeverity::Critical);
    }

    #[tokio::test]
    async fn below_threshold_does_not_dispatch() {
        let (engine, mock) = engine_with_mock(FindingSeverity::Critical, false);
        let delivered = engine
            .notify_report(&report(vec![finding(FindingSeverity::Medium)], false))
            .await;
        assert_eq!(delivered, 0);
        assert_eq!(mock.count(), 0);
    }

    #[test]
    fn from_config_disabled_returns_none() {
        let mut cfg = NotificationConfig::default();
        cfg.enabled = false;
        assert!(NotificationEngine::from_config(&cfg).is_none());
    }

    #[test]
    fn from_config_builds_all_real_providers() {
        let mut cfg = NotificationConfig::default();
        cfg.enabled = true;
        cfg.min_severity = "high".to_string();
        cfg.notify_on_escalation = true;
        cfg.slack = Some(SlackConfig {
            webhook_url: "https://hooks.slack.example/x".to_string(),
        });
        cfg.teams = Some(TeamsConfig {
            webhook_url: "https://teams.example/x".to_string(),
        });
        cfg.pagerduty = Some(PagerDutyConfig {
            routing_key: "routing-key-123".to_string(),
        });

        let engine = NotificationEngine::from_config(&cfg).expect("enabled => Some");
        // min_severity parsed as High: High notifies, Medium does not.
        assert!(engine.should_notify(&report(vec![finding(FindingSeverity::High)], false)));
        assert!(!engine.should_notify(&report(vec![finding(FindingSeverity::Medium)], false)));
    }

    #[test]
    fn from_config_without_providers_installs_mock() {
        let mut cfg = NotificationConfig::default();
        cfg.enabled = true;
        cfg.min_severity = "info".to_string();
        cfg.slack = None;
        cfg.teams = None;
        cfg.pagerduty = None;

        // No real provider configured => falls back to the logging mock, still Some.
        let engine = NotificationEngine::from_config(&cfg).expect("enabled => Some");
        assert!(engine.should_notify(&report(vec![finding(FindingSeverity::Info)], false)));
    }
}
