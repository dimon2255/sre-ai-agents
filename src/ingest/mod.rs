//! Ingestion adapters that translate external transports into `AlertInfo`
//! and enqueue onto the orchestrator's `mpsc::Sender<AlertInfo>` (ADR-013).
//!
//! The architectural seam for this module is `mpsc::Sender<AlertInfo>`.
//! Everything in this module is **above the seam** (boundary). Nothing here
//! should know about agents, the orchestrator workflow, or the store.

pub mod metrics;
pub mod nats;

use tokio::sync::mpsc;
use tracing::warn;

use sre_azure::alerts_client::AzureAlertsClient;
use sre_azure::models::AzureCommonAlert;
use sre_common::config::PlatformConfig;
use sre_common::messages::AlertInfo;

/// Outcome of accepting an alert payload for investigation.
///
/// Variants represent transport-agnostic states. Each transport adapter
/// (webhook, NATS) maps these to its own protocol semantics:
///
/// | Outcome      | Webhook    | NATS                  |
/// |--------------|------------|-----------------------|
/// | `Accepted`   | 202        | `ack()`               |
/// | `Ignored`    | 200        | `ack()`               |
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestOutcome {
    /// Alert was parsed, was not self-referential, and was enqueued for
    /// investigation. The transport should acknowledge upstream.
    Accepted,
    /// Alert was parsed but matches `webhook.self_resource_id` — the
    /// self-exclusion filter that breaks the OTLP feedback loop. The
    /// transport should acknowledge upstream (we accepted it, just
    /// chose not to act on it).
    Ignored,
}

/// Failure modes when accepting an alert payload.
///
/// Each variant has well-defined transport semantics:
///
/// | Error           | Webhook    | NATS                                        |
/// |-----------------|------------|---------------------------------------------|
/// | `Parse`         | 400        | `term()` (poison; do not redeliver)         |
/// | `ChannelFull`   | 503        | no-ack (let `ack_wait` expire — redeliver)  |
/// | `ChannelClosed` | 503 + log  | no-ack + exit ingest loop (fatal)           |
#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    /// Payload could not be deserialized or did not match the Azure
    /// Common Alert Schema.
    #[error("alert payload parse failed: {0}")]
    Parse(String),
    /// The `alert_tx` channel is at capacity. The caller should signal
    /// backpressure (HTTP 503 / no-ack) so the upstream retries.
    #[error("alert channel full")]
    ChannelFull,
    /// The `alert_tx` channel is closed — the orchestrator dispatch loop
    /// has terminated. Fatal for ingestion.
    #[error("alert channel closed")]
    ChannelClosed,
}

/// Parse a serialized Azure Common Alert Schema payload and, if valid and
/// not self-referential, enqueue it on `alert_tx`.
///
/// This is the single point where the platform decides whether to take
/// ownership of an alert. Both the HTTP webhook and the NATS JetStream
/// ingestor route through here so the self-exclusion filter and the
/// backpressure contract live in exactly one place (ADR-013).
pub fn parse_and_enqueue(
    payload: &AzureCommonAlert,
    alert_tx: &mpsc::Sender<AlertInfo>,
    config: &PlatformConfig,
) -> Result<IngestOutcome, IngestError> {
    let alert_info = AzureAlertsClient::parse_common_alert(payload)
        .map_err(|e| IngestError::Parse(e.to_string()))?;

    // Azure delivers both "Fired" and "Resolved" notifications through the same
    // webhook/action group. Running an investigation for a "Resolved" condition
    // would page for something that has already recovered, so acknowledge it
    // upstream but don't act on it. Unknown/empty conditions still investigate.
    if alert_info
        .monitor_condition
        .eq_ignore_ascii_case("Resolved")
    {
        warn!(
            alert_id = %alert_info.alert_id,
            condition = %alert_info.monitor_condition,
            "Ignoring resolved alert notification — condition already recovered"
        );
        return Ok(IngestOutcome::Ignored);
    }

    if let Some(ref self_id) = config.webhook.self_resource_id {
        // Azure resource IDs are case-insensitive by ARM contract
        // (Microsoft.Web vs microsoft.web are the same resource). Compare
        // case-insensitively so a casing-only mismatch in config doesn't
        // silently bypass the self-exclusion filter (OTLP feedback loop).
        if alert_info.affected_resource.eq_ignore_ascii_case(self_id) {
            warn!(
                alert_id = %alert_info.alert_id,
                resource = %alert_info.affected_resource,
                "Ignoring self-referential alert — breaks OTLP feedback loop"
            );
            return Ok(IngestOutcome::Ignored);
        }
    }

    match alert_tx.try_send(alert_info) {
        Ok(()) => Ok(IngestOutcome::Accepted),
        Err(mpsc::error::TrySendError::Full(_)) => Err(IngestError::ChannelFull),
        Err(mpsc::error::TrySendError::Closed(_)) => Err(IngestError::ChannelClosed),
    }
}

/// Parse a raw byte payload into an `AzureCommonAlert` and delegate to
/// `parse_and_enqueue`. Used by transports that hand us bytes rather than
/// a pre-deserialized struct (e.g., NATS message bodies).
pub fn parse_bytes_and_enqueue(
    bytes: &[u8],
    alert_tx: &mpsc::Sender<AlertInfo>,
    config: &PlatformConfig,
) -> Result<IngestOutcome, IngestError> {
    let payload: AzureCommonAlert = serde_json::from_slice(bytes)
        .map_err(|e| IngestError::Parse(format!("json decode: {e}")))?;
    parse_and_enqueue(&payload, alert_tx, config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::config::PlatformConfig;

    fn fixture_payload() -> &'static [u8] {
        br#"{
            "schemaId": "azureMonitorCommonAlertSchema",
            "data": {
                "essentials": {
                    "alertId": "/subscriptions/sub/providers/Microsoft.AlertsManagement/alerts/test-alert-1",
                    "alertRule": "Test Rule",
                    "severity": "Sev2",
                    "signalType": "Metric",
                    "monitorCondition": "Fired",
                    "alertTargetIDs": ["/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Web/sites/my-app"],
                    "firedDateTime": "2026-05-18T12:00:00Z"
                },
                "alertContext": {}
            }
        }"#
    }

    #[tokio::test]
    async fn parse_bytes_and_enqueue_accepts_valid_payload() {
        let (tx, mut rx) = mpsc::channel(8);
        let config = PlatformConfig::default();

        let outcome = parse_bytes_and_enqueue(fixture_payload(), &tx, &config).unwrap();
        assert_eq!(outcome, IngestOutcome::Accepted);

        let received = rx.try_recv().expect("alert should land in channel");
        assert_eq!(received.alert_rule, "Test Rule");
    }

    #[tokio::test]
    async fn parse_bytes_and_enqueue_rejects_malformed_json() {
        let (tx, _rx) = mpsc::channel(8);
        let config = PlatformConfig::default();

        let err = parse_bytes_and_enqueue(b"not json", &tx, &config).unwrap_err();
        assert!(matches!(err, IngestError::Parse(_)));
    }

    #[tokio::test]
    async fn parse_bytes_and_enqueue_returns_channel_full() {
        let (tx, _rx) = mpsc::channel(1);
        let config = PlatformConfig::default();

        // Fill the channel
        let _ = parse_bytes_and_enqueue(fixture_payload(), &tx, &config).unwrap();
        // Second send must signal backpressure
        let err = parse_bytes_and_enqueue(fixture_payload(), &tx, &config).unwrap_err();
        assert!(matches!(err, IngestError::ChannelFull));
    }

    #[tokio::test]
    async fn parse_bytes_and_enqueue_returns_channel_closed() {
        let (tx, rx) = mpsc::channel(8);
        drop(rx);
        let config = PlatformConfig::default();

        let err = parse_bytes_and_enqueue(fixture_payload(), &tx, &config).unwrap_err();
        assert!(matches!(err, IngestError::ChannelClosed));
    }

    #[tokio::test]
    async fn self_referential_alert_is_ignored_but_acknowledged() {
        let (tx, mut rx) = mpsc::channel(8);
        let mut config = PlatformConfig::default();
        config.webhook.self_resource_id = Some(
            "/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Web/sites/my-app".to_string(),
        );

        let outcome = parse_bytes_and_enqueue(fixture_payload(), &tx, &config).unwrap();
        assert_eq!(outcome, IngestOutcome::Ignored);
        assert!(
            rx.try_recv().is_err(),
            "no alert should land in the channel"
        );
    }

    #[tokio::test]
    async fn self_referential_match_is_case_insensitive() {
        // Azure resource IDs are case-insensitive by ARM contract.
        // Fixture's affected_resource has `Microsoft.Web/sites/my-app`;
        // a self_resource_id with `microsoft.web/sites/my-app` must match.
        let (tx, mut rx) = mpsc::channel(8);
        let mut config = PlatformConfig::default();
        config.webhook.self_resource_id = Some(
            "/SUBSCRIPTIONS/sub/resourcegroups/rg/providers/microsoft.web/sites/MY-APP".to_string(),
        );

        let outcome = parse_bytes_and_enqueue(fixture_payload(), &tx, &config).unwrap();
        assert_eq!(outcome, IngestOutcome::Ignored);
        assert!(rx.try_recv().is_err());
    }
}
