use std::time::Duration;

use async_trait::async_trait;

use crate::notification::Notification;
use crate::traits::{Notifier, NotifyError};

/// Posts to a Slack incoming webhook. The message body uses Slack mrkdwn.
pub struct SlackNotifier {
    http: reqwest::Client,
    webhook_url: String,
}

impl SlackNotifier {
    pub fn new(webhook_url: String) -> Self {
        Self {
            // A notifier with no request timeout can hang forever on a
            // blackholed proxy; because notify runs while the investigation
            // holds its concurrency permit, that would leak permits and wedge
            // the dispatch loop. Bound every send.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            webhook_url,
        }
    }

    /// Render the notification as Slack mrkdwn text.
    pub fn render(notification: &Notification) -> String {
        let mut text = format!(
            "*{}*\n{}\nResource: `{}`  •  Severity: *{}*  •  Confidence: {:.0}%  •  Findings: {}",
            notification.title,
            notification.summary,
            notification.affected_resource,
            notification.severity,
            notification.overall_confidence * 100.0,
            notification.finding_count,
        );
        for f in &notification.top_findings {
            text.push_str(&format!("\n• {f}"));
        }
        text
    }
}

#[async_trait]
impl Notifier for SlackNotifier {
    fn name(&self) -> &str {
        "slack"
    }

    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        let body = serde_json::json!({ "text": Self::render(notification) });
        let resp = self
            .http
            .post(&self.webhook_url)
            .json(&body)
            .send()
            .await
            // without_url() strips the webhook URL from the error — the Slack
            // webhook secret lives in that URL's path and must never be logged.
            .map_err(|e| NotifyError::Http(e.without_url().to_string()))?;

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
    use sre_common::findings::FindingSeverity;

    fn sample() -> Notification {
        Notification {
            investigation_id: "id-1".to_string(),
            title: "SRE investigation escalated — payments-api".to_string(),
            summary: "2 findings".to_string(),
            severity: FindingSeverity::Critical,
            affected_resource: "payments-api".to_string(),
            overall_confidence: 0.91,
            escalated: true,
            finding_count: 2,
            top_findings: vec!["[CRITICAL] Error spike".to_string()],
            occurred_at: Utc::now(),
        }
    }

    #[test]
    fn render_includes_key_fields() {
        let text = SlackNotifier::render(&sample());
        assert!(text.contains("payments-api"));
        assert!(text.contains("CRITICAL"));
        assert!(text.contains("91%"));
        assert!(text.contains("Error spike"));
    }
}
