use std::time::Duration;

use async_trait::async_trait;

use sre_common::findings::FindingSeverity;

use crate::notification::Notification;
use crate::traits::{Notifier, NotifyError};

const PAGERDUTY_EVENTS_URL: &str = "https://events.pagerduty.com/v2/enqueue";

/// Triggers a PagerDuty incident via the Events API v2. The `routing_key` is
/// the integration key of a PagerDuty service's Events API v2 integration.
pub struct PagerDutyNotifier {
    http: reqwest::Client,
    routing_key: String,
}

impl PagerDutyNotifier {
    pub fn new(routing_key: String) -> Self {
        Self {
            // Bounded so a hung PagerDuty endpoint cannot hold the
            // investigation's concurrency permit open indefinitely.
            http: sre_common::http::client_builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| {
                    sre_common::http::client_builder()
                        .build()
                        .expect("reqwest client")
                }),
            routing_key,
        }
    }

    /// Build the Events API v2 `trigger` payload. `dedup_key` is the
    /// investigation id so repeat alerts for one investigation collapse onto
    /// a single PagerDuty incident.
    pub fn render(&self, notification: &Notification) -> serde_json::Value {
        serde_json::json!({
            "routing_key": self.routing_key,
            "event_action": "trigger",
            "dedup_key": notification.investigation_id,
            "payload": {
                "summary": notification.title,
                "source": notification.affected_resource,
                "severity": pd_severity(notification.severity),
                "custom_details": {
                    "summary": notification.summary,
                    "confidence": notification.overall_confidence,
                    "escalated": notification.escalated,
                    "finding_count": notification.finding_count,
                    "top_findings": notification.top_findings,
                    "investigation_id": notification.investigation_id,
                },
            },
        })
    }
}

/// Map platform severity to PagerDuty's severity vocabulary.
fn pd_severity(severity: FindingSeverity) -> &'static str {
    match severity {
        FindingSeverity::Critical => "critical",
        FindingSeverity::High => "error",
        FindingSeverity::Medium => "warning",
        FindingSeverity::Low | FindingSeverity::Info => "info",
    }
}

#[async_trait]
impl Notifier for PagerDutyNotifier {
    fn name(&self) -> &str {
        "pagerduty"
    }

    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        let resp = self
            .http
            .post(PAGERDUTY_EVENTS_URL)
            .json(&self.render(notification))
            .send()
            .await
            .map_err(|e| NotifyError::Http(e.to_string()))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let message = resp.text().await.unwrap_or_default();
            return Err(NotifyError::Provider { status, message });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn render_maps_severity_and_sets_dedup_key() {
        let pd = PagerDutyNotifier::new("routing-key-123".to_string());
        let n = Notification {
            investigation_id: "inv-7".to_string(),
            title: "SRE investigation escalated — db".to_string(),
            summary: "summary".to_string(),
            severity: FindingSeverity::High,
            affected_resource: "db".to_string(),
            overall_confidence: 0.8,
            escalated: true,
            finding_count: 3,
            top_findings: vec![],
            occurred_at: Utc::now(),
        };
        let body = pd.render(&n);
        assert_eq!(body["event_action"], "trigger");
        assert_eq!(body["dedup_key"], "inv-7");
        assert_eq!(body["payload"]["severity"], "error"); // High -> error
        assert_eq!(body["routing_key"], "routing-key-123");
    }
}
