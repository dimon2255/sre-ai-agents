use std::time::Duration;

use async_trait::async_trait;

use sre_common::findings::FindingSeverity;

use crate::notification::Notification;
use crate::traits::{Notifier, NotifyError};

/// Posts to a Microsoft Teams incoming webhook using the legacy MessageCard
/// format (broadly compatible with Office 365 connector webhooks).
pub struct TeamsNotifier {
    http: reqwest::Client,
    webhook_url: String,
}

impl TeamsNotifier {
    pub fn new(webhook_url: String) -> Self {
        Self {
            // Bounded so a hung Teams webhook cannot hold the investigation's
            // concurrency permit open indefinitely.
            http: sre_common::http::client_builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap_or_else(|_| {
                    sre_common::http::client_builder()
                        .build()
                        .expect("reqwest client")
                }),
            webhook_url,
        }
    }

    /// Build the MessageCard JSON body.
    pub fn render(notification: &Notification) -> serde_json::Value {
        let mut text = notification.summary.clone();
        for f in &notification.top_findings {
            text.push_str(&format!("\n\n- {f}"));
        }

        serde_json::json!({
            "@type": "MessageCard",
            "@context": "http://schema.org/extensions",
            "themeColor": theme_color(notification.severity),
            "summary": notification.title,
            "title": notification.title,
            "sections": [{
                "activitySubtitle": notification.affected_resource,
                "facts": [
                    { "name": "Severity", "value": notification.severity.to_string() },
                    { "name": "Confidence", "value": format!("{:.0}%", notification.overall_confidence * 100.0) },
                    { "name": "Findings", "value": notification.finding_count.to_string() },
                    { "name": "Escalated", "value": if notification.escalated { "yes" } else { "no" } },
                    { "name": "Investigation", "value": notification.investigation_id.clone() },
                ],
                "text": text,
            }],
        })
    }
}

fn theme_color(severity: FindingSeverity) -> &'static str {
    match severity {
        FindingSeverity::Critical => "D32F2F",
        FindingSeverity::High => "F57C00",
        FindingSeverity::Medium => "FBC02D",
        FindingSeverity::Low => "388E3C",
        FindingSeverity::Info => "1976D2",
    }
}

#[async_trait]
impl Notifier for TeamsNotifier {
    fn name(&self) -> &str {
        "teams"
    }

    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        let resp = self
            .http
            .post(&self.webhook_url)
            .json(&Self::render(notification))
            .send()
            .await
            // without_url() strips the webhook URL from the error — the Teams
            // webhook secret lives in that URL and must never be logged.
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

    #[test]
    fn render_is_a_message_card_with_facts() {
        let n = Notification {
            investigation_id: "id-9".to_string(),
            title: "SRE investigation (HIGH) — web-app".to_string(),
            summary: "summary".to_string(),
            severity: FindingSeverity::High,
            affected_resource: "web-app".to_string(),
            overall_confidence: 0.7,
            escalated: false,
            finding_count: 1,
            top_findings: vec!["[HIGH] Latency".to_string()],
            occurred_at: Utc::now(),
        };
        let card = TeamsNotifier::render(&n);
        assert_eq!(card["@type"], "MessageCard");
        assert_eq!(card["themeColor"], "F57C00");
        assert_eq!(card["sections"][0]["activitySubtitle"], "web-app");
    }
}
