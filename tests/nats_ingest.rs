//! End-to-end smoke test for the NATS JetStream ingestor (ADR-013).
//!
//! Spins up a real NATS server via testcontainers, publishes an Azure
//! Common Alert Schema payload to the configured subject, and asserts
//! the ingestor decodes it into an `AlertInfo` and forwards it onto
//! the shared `mpsc::Sender<AlertInfo>` channel.
//!
//! Docker must be available for this test to run, consistent with the
//! existing Postgres integration tests in `crates/store/tests`.

use std::sync::atomic::AtomicU8;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use futures_util::StreamExt;
use sre_agents_platform::ingest::nats as ingest_nats;
use sre_common::config::{NatsConfig, PlatformConfig};
use sre_common::messages::AlertInfo;
use testcontainers_modules::nats::{Nats, NatsServerCmd};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ImageExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const SUBJECT: &str = "alerts.azure.test";
const FIXTURE_PAYLOAD: &[u8] = br#"{
    "schemaId": "azureMonitorCommonAlertSchema",
    "data": {
        "essentials": {
            "alertId": "/subscriptions/sub/providers/Microsoft.AlertsManagement/alerts/nats-smoke-1",
            "alertRule": "Smoke Test Rule",
            "severity": "Sev2",
            "signalType": "Metric",
            "monitorCondition": "Fired",
            "alertTargetIDs": ["/subscriptions/sub/resourceGroups/rg/providers/Microsoft.Web/sites/smoke-target"],
            "firedDateTime": "2026-05-18T12:00:00Z"
        },
        "alertContext": {}
    }
}"#;

#[tokio::test]
async fn nats_ingestor_round_trip() {
    // JetStream is opt-in on the NATS server — the default `nats` image
    // does not enable it. Without `--jetstream`, any JetStream API call
    // (stream create, publish ack, consumer create) times out.
    let nats_cmd = NatsServerCmd::default().with_jetstream();
    let container = Nats::default()
        .with_cmd(&nats_cmd)
        .start()
        .await
        .expect("failed to start NATS container");
    let port = container
        .get_host_port_ipv4(4222)
        .await
        .expect("failed to map NATS port");
    let url = format!("nats://127.0.0.1:{port}");

    let nats_cfg = NatsConfig {
        enabled: true,
        url: url.clone(),
        credentials_path: None,
        stream_name: "sre-alerts-test".to_string(),
        consumer_name: "test-consumer".to_string(),
        subject_filter: "alerts.azure.>".to_string(),
        max_ack_pending: 8,
        ack_wait_secs: 5,
        max_deliver: 3,
        max_age_hours: 1,
        num_replicas: 1,
        dlq_enabled: true,
        dlq_stream_name: "sre-alerts-dlq-test".to_string(),
        dlq_subject_filter: "alerts.dlq.azure.>".to_string(),
        dlq_subject: "alerts.dlq.azure.poison".to_string(),
        dlq_max_age_hours: 1,
    };
    let platform_cfg = PlatformConfig::default();

    let (alert_tx, mut alert_rx) = mpsc::channel::<AlertInfo>(8);
    let cancel = CancellationToken::new();

    // Pre-create the JetStream stream AND the consumer from the test
    // side BEFORE publishing. Two races to remove:
    //
    // 1. StreamNotFound on publish ack: `jetstream.publish()` returns
    //    `Ok` at the network level even when the stream doesn't exist;
    //    the failure surfaces only when the ack future resolves.
    //
    // 2. DeliverPolicy::New skip: the production ingestor creates its
    //    consumer with `DeliverPolicy::New`, which means "only messages
    //    that arrive after the consumer exists." If the publish happens
    //    before the ingestor calls `get_or_create_consumer`, the message
    //    is in the stream but invisible to the consumer.
    //
    // The ingestor's `get_or_create_*` calls are idempotent, so reusing
    // pre-created resources mirrors what would happen on a normal
    // ingestor restart against a long-lived stream.
    use async_nats::jetstream::consumer;
    let publisher = async_nats::connect(&url)
        .await
        .expect("publisher connect failed");
    let jetstream = async_nats::jetstream::new(publisher);
    let stream = jetstream
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: nats_cfg.stream_name.clone(),
            subjects: vec![nats_cfg.subject_filter.clone()],
            ..Default::default()
        })
        .await
        .expect("pre-create stream failed");
    stream
        .get_or_create_consumer(
            &nats_cfg.consumer_name,
            consumer::pull::Config {
                durable_name: Some(nats_cfg.consumer_name.clone()),
                ack_policy: consumer::AckPolicy::Explicit,
                filter_subject: nats_cfg.subject_filter.clone(),
                deliver_policy: consumer::DeliverPolicy::New,
                ..Default::default()
            },
        )
        .await
        .expect("pre-create consumer failed");

    // Spawn the ingestor — it finds the stream + consumer exist and
    // attaches its message-pull loop.
    let ingestor_cancel = cancel.clone();
    let ingestor_cfg = nats_cfg.clone();
    let ingestor_platform = platform_cfg.clone();
    let ingestor_status = Arc::new(AtomicU8::new(
        ingest_nats::NatsIngestorState::Disabled as u8,
    ));
    let ingestor_handle = tokio::spawn(async move {
        ingest_nats::run(
            ingestor_cfg,
            ingestor_platform,
            alert_tx,
            ingestor_cancel,
            ingestor_status,
        )
        .await
    });

    // Publish the fixture alert. Stream is guaranteed to exist; no retry needed.
    let payload = Bytes::from_static(FIXTURE_PAYLOAD);
    let ack = jetstream
        .publish(SUBJECT, payload)
        .await
        .expect("publish call failed");
    ack.await.expect("publish ack failed");

    // Receive the AlertInfo on the orchestrator side.
    let received = tokio::time::timeout(Duration::from_secs(10), alert_rx.recv())
        .await
        .expect("did not receive AlertInfo within 10s")
        .expect("alert channel closed unexpectedly");

    assert_eq!(received.alert_rule, "Smoke Test Rule");
    assert!(
        received.affected_resource.ends_with("/smoke-target"),
        "unexpected affected_resource: {}",
        received.affected_resource
    );
    assert_eq!(received.signal_type, "Metric");

    // Graceful shutdown: trip the cancel token, wait for the ingestor task.
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), ingestor_handle)
        .await
        .expect("ingestor did not stop within 5s after cancel")
        .expect("ingestor task join failure")
        .expect("ingestor returned error on shutdown");
}

/// A payload that fails to parse as an Azure Common Alert Schema must be
/// routed to the dead-letter stream (ADR-013 PR-3) — not dropped, and not
/// forwarded to the orchestrator channel.
#[tokio::test]
async fn nats_ingestor_routes_poison_to_dlq() {
    use async_nats::jetstream::consumer;

    let nats_cmd = NatsServerCmd::default().with_jetstream();
    let container = Nats::default()
        .with_cmd(&nats_cmd)
        .start()
        .await
        .expect("failed to start NATS container");
    let port = container
        .get_host_port_ipv4(4222)
        .await
        .expect("failed to map NATS port");
    let url = format!("nats://127.0.0.1:{port}");

    let nats_cfg = NatsConfig {
        enabled: true,
        url: url.clone(),
        credentials_path: None,
        stream_name: "sre-alerts-poison".to_string(),
        consumer_name: "poison-consumer".to_string(),
        subject_filter: "alerts.azure.>".to_string(),
        max_ack_pending: 8,
        ack_wait_secs: 5,
        max_deliver: 3,
        max_age_hours: 1,
        num_replicas: 1,
        dlq_enabled: true,
        dlq_stream_name: "sre-alerts-dlq-poison".to_string(),
        dlq_subject_filter: "alerts.dlq.azure.>".to_string(),
        dlq_subject: "alerts.dlq.azure.poison".to_string(),
        dlq_max_age_hours: 1,
    };
    let platform_cfg = PlatformConfig::default();

    let (alert_tx, mut alert_rx) = mpsc::channel::<AlertInfo>(8);
    let cancel = CancellationToken::new();

    let publisher = async_nats::connect(&url)
        .await
        .expect("publisher connect failed");
    let jetstream = async_nats::jetstream::new(publisher);

    // Main stream + consumer (DeliverPolicy::New) pre-created to avoid the
    // publish-before-consumer race, same as the round-trip test.
    let stream = jetstream
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: nats_cfg.stream_name.clone(),
            subjects: vec![nats_cfg.subject_filter.clone()],
            ..Default::default()
        })
        .await
        .expect("pre-create stream failed");
    stream
        .get_or_create_consumer(
            &nats_cfg.consumer_name,
            consumer::pull::Config {
                durable_name: Some(nats_cfg.consumer_name.clone()),
                ack_policy: consumer::AckPolicy::Explicit,
                filter_subject: nats_cfg.subject_filter.clone(),
                deliver_policy: consumer::DeliverPolicy::New,
                ..Default::default()
            },
        )
        .await
        .expect("pre-create consumer failed");

    // Pre-create the DLQ stream + a reader consumer so the test can observe
    // what the ingestor routes there. The ingestor also ensures the DLQ
    // stream idempotently; pre-creating avoids a race on the read side.
    let dlq_stream = jetstream
        .get_or_create_stream(async_nats::jetstream::stream::Config {
            name: nats_cfg.dlq_stream_name.clone(),
            subjects: vec![nats_cfg.dlq_subject_filter.clone()],
            ..Default::default()
        })
        .await
        .expect("pre-create DLQ stream failed");
    let dlq_consumer = dlq_stream
        .get_or_create_consumer(
            "dlq-reader",
            consumer::pull::Config {
                durable_name: Some("dlq-reader".to_string()),
                ack_policy: consumer::AckPolicy::Explicit,
                filter_subject: nats_cfg.dlq_subject_filter.clone(),
                deliver_policy: consumer::DeliverPolicy::All,
                ..Default::default()
            },
        )
        .await
        .expect("pre-create DLQ consumer failed");

    // Spawn the ingestor.
    let ingestor_cancel = cancel.clone();
    let ingestor_cfg = nats_cfg.clone();
    let ingestor_platform = platform_cfg.clone();
    let ingestor_status = Arc::new(AtomicU8::new(
        ingest_nats::NatsIngestorState::Disabled as u8,
    ));
    let ingestor_handle = tokio::spawn(async move {
        ingest_nats::run(
            ingestor_cfg,
            ingestor_platform,
            alert_tx,
            ingestor_cancel,
            ingestor_status,
        )
        .await
    });

    // Publish a payload that is valid JSON but not a Common Alert Schema —
    // the ingestor must fail to parse it (poison) and route it to the DLQ.
    let poison = Bytes::from_static(b"{\"this\":\"is not an azure alert\"}");
    let ack = jetstream
        .publish(SUBJECT, poison.clone())
        .await
        .expect("poison publish call failed");
    ack.await.expect("poison publish ack failed");

    // The poison payload must appear in the DLQ with provenance headers.
    let mut dlq_messages = dlq_consumer
        .messages()
        .await
        .expect("DLQ messages stream failed");
    let dlq_msg = tokio::time::timeout(Duration::from_secs(15), dlq_messages.next())
        .await
        .expect("poison message did not reach DLQ within 15s")
        .expect("DLQ message stream ended")
        .expect("DLQ message error");

    assert_eq!(
        &dlq_msg.payload[..],
        &poison[..],
        "DLQ payload should be the original poison bytes"
    );
    let reason = dlq_msg
        .headers
        .as_ref()
        .and_then(|h| h.get("Sre-Dlq-Reason"));
    assert!(
        reason.is_some(),
        "DLQ message should carry the Sre-Dlq-Reason header"
    );
    let _ = dlq_msg.ack().await;

    // Nothing should reach the orchestrator channel.
    assert!(
        alert_rx.try_recv().is_err(),
        "poison message must not reach the orchestrator"
    );

    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), ingestor_handle)
        .await
        .expect("ingestor did not stop within 5s after cancel")
        .expect("ingestor task join failure")
        .expect("ingestor returned error on shutdown");
}
