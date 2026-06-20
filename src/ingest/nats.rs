//! NATS JetStream ingestor (ADR-013).
//!
//! Subscribes to a durable JetStream pull consumer, decodes each message
//! as `AzureCommonAlert`, and enqueues onto the shared `mpsc::Sender<AlertInfo>`.
//!
//! ## Backpressure contract
//!
//! A message is acked **only after** the channel accepts it. If the channel
//! is full, the message is intentionally not acked — JetStream redelivers
//! it after `ack_wait`. Backpressure from the orchestrator therefore
//! surfaces as redelivery latency rather than message loss.
//!
//! ## Poison handling
//!
//! Messages that fail to parse are terminated (`AckKind::Term`) so they
//! do not consume redelivery budget. Once PR-3 lands, these will be
//! routed to `alerts.dlq.azure.>`.
//!
//! ## Subscription resilience
//!
//! `consumer.messages()` can yield `None` on a transient JetStream API hiccup.
//! The fetch loop is wrapped in an outer subscription loop that rebuilds
//! the stream/consumer subscription with backoff rather than exiting the
//! ingestor task. The task only stops when the shared cancellation token
//! fires (graceful shutdown) or the alert channel is permanently closed
//! (orchestrator stopped — fatal, ACA will restart the container).
//!
//! ## Status reporting
//!
//! A shared `AtomicU8` exposes the ingestor's current state to other
//! components (notably `/readyz`). The webhook is the safety net, so the
//! probe reports NATS state without failing.

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_nats::jetstream::{self, consumer, AckKind};
use async_nats::{ConnectOptions, Event, HeaderMap};
use futures_util::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use sre_common::config::{NatsConfig, PlatformConfig};
use sre_common::messages::AlertInfo;

use super::metrics::NatsMetrics;
use super::{parse_bytes_and_enqueue, IngestError, IngestOutcome};

/// Public state of the NATS ingestor task. Encoded as `AtomicU8` so it
/// can be cheaply shared with the webhook layer for `/readyz` reporting.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatsIngestorState {
    /// Feature flag is off — task never spawned.
    Disabled = 0,
    /// Task spawned, attempting initial connection / subscription.
    Connecting = 1,
    /// Connected and pulling messages.
    Running = 2,
    /// Lost connection or subscription; rebuilding with backoff.
    Reconnecting = 3,
    /// Task has exited (cancel requested or fatal alert-channel closed).
    Stopped = 4,
}

impl NatsIngestorState {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connecting,
            2 => Self::Running,
            3 => Self::Reconnecting,
            4 => Self::Stopped,
            _ => Self::Disabled,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connecting => "connecting",
            Self::Running => "running",
            Self::Reconnecting => "reconnecting",
            Self::Stopped => "stopped",
        }
    }
}

fn set_state(status: &AtomicU8, state: NatsIngestorState) {
    status.store(state as u8, Ordering::Relaxed);
}

/// Backoff between subscription rebuild attempts.
const SUBSCRIPTION_BACKOFF: Duration = Duration::from_secs(2);
/// Preflight connect deadline — caller refuses to start above this.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);

/// Quick connection test used at startup (#10). Opens a NATS connection,
/// then drops it. Bounded by `PREFLIGHT_TIMEOUT` so a hung DNS or
/// unreachable server doesn't block bootstrap indefinitely.
pub async fn preflight(cfg: &NatsConfig) -> Result<()> {
    let opts = build_options(cfg).await?;
    tokio::time::timeout(PREFLIGHT_TIMEOUT, opts.connect(&cfg.url))
        .await
        .with_context(|| format!("NATS preflight to {} timed out", cfg.url))?
        .with_context(|| format!("NATS preflight to {} failed", cfg.url))?;
    Ok(())
}

/// Build connection options from config. Shared between `preflight` and
/// the long-lived `run` connection so credentials handling lives in one
/// place.
async fn build_options(cfg: &NatsConfig) -> Result<ConnectOptions> {
    match &cfg.credentials_path {
        Some(path) => async_nats::ConnectOptions::with_credentials_file(path)
            .await
            .with_context(|| format!("failed to load NATS credentials from {path}")),
        None => {
            warn!(
                "NATS connecting without credentials — acceptable for local dev only. \
                 Set nats.credentials_path in staging/prod."
            );
            Ok(async_nats::ConnectOptions::new())
        }
    }
}

/// Run the NATS JetStream ingestor until the cancellation token fires
/// or a fatal error occurs (e.g., the orchestrator's alert channel
/// closes permanently — see `MessageAction::FatalChannelClosed`).
pub async fn run(
    cfg: NatsConfig,
    platform_cfg: PlatformConfig,
    alert_tx: mpsc::Sender<AlertInfo>,
    cancel: CancellationToken,
    status: Arc<AtomicU8>,
) -> Result<()> {
    set_state(&status, NatsIngestorState::Connecting);

    let client = connect(&cfg, status.clone()).await?;
    let jetstream = jetstream::new(client);

    info!(
        url = %cfg.url,
        stream = %cfg.stream_name,
        consumer = %cfg.consumer_name,
        subject = %cfg.subject_filter,
        max_ack_pending = cfg.max_ack_pending,
        ack_wait_secs = cfg.ack_wait_secs,
        "NATS JetStream ingestor started"
    );

    // Per-message OTel metrics (PR-2). No-ops when observability is disabled.
    let metrics = NatsMetrics::new();

    // Dead-letter publisher (PR-3). The DLQ stream itself is ensured inside
    // the outer loop (idempotent get_or_create) so a reconnect re-asserts it.
    let dlq = if cfg.dlq_enabled {
        Some(Dlq {
            js: jetstream.clone(),
            subject: cfg.dlq_subject.clone(),
        })
    } else {
        info!(
            "NATS DLQ disabled (nats.dlq_enabled=false) — poison messages are terminated, not retained"
        );
        None
    };

    // Outer loop: rebuild stream/consumer/subscription on transient end.
    // `messages.next() == None` is treated as transient — async-nats can
    // surface a closed message stream on a brief JetStream API hiccup,
    // and we want the ingestor to recover rather than exit silently.
    loop {
        if cancel.is_cancelled() {
            break;
        }

        let stream = match ensure_stream(&jetstream, &cfg).await {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "ensure_stream failed — backing off and retrying");
                set_state(&status, NatsIngestorState::Reconnecting);
                if backoff_or_cancel(&cancel).await {
                    break;
                }
                continue;
            }
        };

        // Ensure the DLQ stream exists (idempotent). A DLQ failure must not
        // stop ingestion — log and proceed; poison handling falls back to
        // terminate-only for this cycle.
        if dlq.is_some() {
            if let Err(e) = ensure_dlq_stream(&jetstream, &cfg).await {
                warn!(error = %e, "ensure_dlq_stream failed — poison messages terminated without DLQ this cycle");
            }
        }

        let consumer = match ensure_consumer(&stream, &cfg).await {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "ensure_consumer failed — backing off and retrying");
                set_state(&status, NatsIngestorState::Reconnecting);
                if backoff_or_cancel(&cancel).await {
                    break;
                }
                continue;
            }
        };

        let mut messages = match consumer.messages().await {
            Ok(m) => m,
            Err(e) => {
                warn!(error = %e, "consumer.messages() failed — backing off and retrying");
                set_state(&status, NatsIngestorState::Reconnecting);
                if backoff_or_cancel(&cancel).await {
                    break;
                }
                continue;
            }
        };

        set_state(&status, NatsIngestorState::Running);

        // Inner loop: pull messages until cancel or stream ends.
        let fatal_close = loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    info!("NATS ingestor cancellation requested — exiting fetch loop");
                    break false;
                }
                next = messages.next() => {
                    match next {
                        Some(Ok(message)) => {
                            match handle_message(
                                message,
                                &platform_cfg,
                                &alert_tx,
                                &metrics,
                                dlq.as_ref(),
                            )
                            .await
                            {
                                MessageAction::Continue => {}
                                MessageAction::FatalChannelClosed => {
                                    // The orchestrator's alert channel has been closed —
                                    // there is no path back to forward progress. Trip the
                                    // shared cancel token so the rest of the platform
                                    // shuts down and ACA restarts the container.
                                    error!(
                                        "alert channel closed — orchestrator stopped; \
                                         tripping shared cancel for clean restart"
                                    );
                                    cancel.cancel();
                                    break true;
                                }
                            }
                        }
                        Some(Err(e)) => {
                            warn!(error = %e, "NATS message stream yielded error — continuing");
                        }
                        None => {
                            warn!(
                                "NATS message stream ended unexpectedly — \
                                 rebuilding subscription"
                            );
                            break false;
                        }
                    }
                }
            }
        };

        if fatal_close || cancel.is_cancelled() {
            break;
        }

        set_state(&status, NatsIngestorState::Reconnecting);
        if backoff_or_cancel(&cancel).await {
            break;
        }
    }

    set_state(&status, NatsIngestorState::Stopped);
    info!("NATS JetStream ingestor stopped");
    Ok(())
}

/// Sleep for `SUBSCRIPTION_BACKOFF` or until cancel — returns `true` if
/// cancel fired (caller should exit), `false` if backoff completed.
async fn backoff_or_cancel(cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => true,
        _ = tokio::time::sleep(SUBSCRIPTION_BACKOFF) => false,
    }
}

/// Long-lived NATS connection with explicit reconnect telemetry (#7).
/// `async-nats` retries forever by default; the event callback surfaces
/// state transitions to the shared status atomic so `/readyz` reflects
/// what the consumer is actually doing.
async fn connect(cfg: &NatsConfig, status: Arc<AtomicU8>) -> Result<async_nats::Client> {
    let opts = build_options(cfg).await?;

    let cb_status = status.clone();
    let opts = opts
        .max_reconnects(None)
        .retry_on_initial_connect()
        .event_callback(move |event| {
            let cb_status = cb_status.clone();
            async move {
                match event {
                    Event::Connected => {
                        info!("NATS event: connected");
                        set_state(&cb_status, NatsIngestorState::Running);
                    }
                    Event::Disconnected => {
                        warn!("NATS event: disconnected — async-nats will retry");
                        set_state(&cb_status, NatsIngestorState::Reconnecting);
                    }
                    Event::LameDuckMode => {
                        warn!("NATS event: server lame-duck mode (planned shutdown)");
                    }
                    Event::Draining => {
                        info!("NATS event: draining");
                    }
                    Event::Closed => {
                        error!("NATS event: connection closed (fatal — no more reconnects)");
                        set_state(&cb_status, NatsIngestorState::Stopped);
                    }
                    Event::SlowConsumer(sid) => {
                        warn!(sid, "NATS event: slow consumer");
                    }
                    Event::ServerError(err) => {
                        warn!(error = %err, "NATS event: server error");
                    }
                    Event::ClientError(err) => {
                        warn!(error = %err, "NATS event: client error");
                    }
                }
            }
        });

    opts.connect(&cfg.url)
        .await
        .with_context(|| format!("failed to connect to NATS at {}", cfg.url))
}

async fn ensure_stream(
    js: &jetstream::Context,
    cfg: &NatsConfig,
) -> Result<jetstream::stream::Stream> {
    js.get_or_create_stream(jetstream::stream::Config {
        name: cfg.stream_name.clone(),
        subjects: vec![cfg.subject_filter.clone()],
        retention: jetstream::stream::RetentionPolicy::Limits,
        storage: jetstream::stream::StorageType::File,
        max_age: Duration::from_secs(cfg.max_age_hours.saturating_mul(3600)),
        max_message_size: 64 * 1024,
        num_replicas: cfg.num_replicas,
        ..Default::default()
    })
    .await
    .context("failed to create or fetch JetStream stream")
}

async fn ensure_consumer(
    stream: &jetstream::stream::Stream,
    cfg: &NatsConfig,
) -> Result<consumer::PullConsumer> {
    stream
        .get_or_create_consumer(
            &cfg.consumer_name,
            consumer::pull::Config {
                durable_name: Some(cfg.consumer_name.clone()),
                ack_policy: consumer::AckPolicy::Explicit,
                ack_wait: Duration::from_secs(cfg.ack_wait_secs),
                max_deliver: cfg.max_deliver,
                filter_subject: cfg.subject_filter.clone(),
                max_ack_pending: cfg.max_ack_pending,
                deliver_policy: consumer::DeliverPolicy::New,
                ..Default::default()
            },
        )
        .await
        .context("failed to create or fetch JetStream consumer")
}

/// Dead-letter publisher (ADR-013 PR-3). Poison messages — payloads that
/// fail to parse as an Azure Common Alert Schema — are republished here for
/// forensic inspection instead of being silently terminated.
struct Dlq {
    js: jetstream::Context,
    subject: String,
}

impl Dlq {
    /// Publish a poison payload to the DLQ subject with provenance headers.
    /// Awaits the JetStream ack so a failure to persist surfaces to the
    /// caller (which logs and proceeds with termination — an unparseable
    /// message must never be redelivered).
    async fn publish(&self, original_subject: &str, payload: &[u8], reason: &str) -> Result<()> {
        let mut headers = HeaderMap::new();
        headers.insert("Sre-Dlq-Reason", reason);
        headers.insert("Sre-Original-Subject", original_subject);
        self.js
            .publish_with_headers(self.subject.clone(), headers, payload.to_vec().into())
            .await
            .context("DLQ publish call failed")?
            .await
            .context("DLQ publish was not acknowledged")?;
        Ok(())
    }
}

/// Ensure the dead-letter stream exists (idempotent). Bound to the DLQ
/// subject wildcard with a wider retention than the main stream so poison
/// messages survive long enough to be investigated.
async fn ensure_dlq_stream(js: &jetstream::Context, cfg: &NatsConfig) -> Result<()> {
    js.get_or_create_stream(jetstream::stream::Config {
        name: cfg.dlq_stream_name.clone(),
        subjects: vec![cfg.dlq_subject_filter.clone()],
        retention: jetstream::stream::RetentionPolicy::Limits,
        storage: jetstream::stream::StorageType::File,
        max_age: Duration::from_secs(cfg.dlq_max_age_hours.saturating_mul(3600)),
        max_message_size: 64 * 1024,
        num_replicas: cfg.num_replicas,
        ..Default::default()
    })
    .await
    .context("failed to create or fetch DLQ stream")?;
    Ok(())
}

/// What `handle_message` instructs the fetch loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MessageAction {
    /// Continue pulling messages.
    Continue,
    /// Orchestrator's alert channel is closed — abandon the ingestor.
    FatalChannelClosed,
}

async fn handle_message(
    message: jetstream::Message,
    platform_cfg: &PlatformConfig,
    alert_tx: &mpsc::Sender<AlertInfo>,
    metrics: &NatsMetrics,
    dlq: Option<&Dlq>,
) -> MessageAction {
    let subject = message.subject.clone();

    // Record per-message delivery telemetry (PR-2). `info()` parses the
    // JetStream reply subject; on the rare parse failure we simply skip
    // metrics for this message rather than fail handling.
    if let Ok(info) = message.info() {
        metrics.record_delivery(info.delivered, info.pending);
    }

    match parse_bytes_and_enqueue(&message.payload, alert_tx, platform_cfg) {
        Ok(IngestOutcome::Accepted) => {
            if let Err(e) = message.ack().await {
                warn!(error = %e, subject = %subject, "failed to ack NATS message");
            }
            MessageAction::Continue
        }
        Ok(IngestOutcome::Ignored) => {
            if let Err(e) = message.ack().await {
                warn!(error = %e, subject = %subject, "failed to ack ignored NATS message");
            }
            MessageAction::Continue
        }
        Err(IngestError::Parse(reason)) => {
            warn!(
                reason = %reason,
                subject = %subject,
                "NATS payload parse failure — routing to DLQ and terminating (poison)"
            );
            // PR-3: republish the poison payload to the DLQ stream before
            // terminating. A DLQ failure is logged but never blocks the
            // terminate — we must not redeliver an unparseable message.
            if let Some(dlq) = dlq {
                match dlq.publish(&subject, &message.payload, &reason).await {
                    Ok(()) => metrics.record_dlq("parse_failure"),
                    Err(e) => {
                        warn!(error = %e, subject = %subject, "failed to route poison message to DLQ")
                    }
                }
            }
            if let Err(e) = message.ack_with(AckKind::Term).await {
                warn!(error = %e, subject = %subject, "failed to term poison NATS message");
            }
            MessageAction::Continue
        }
        Err(IngestError::ChannelFull) => {
            debug!(
                subject = %subject,
                "alert channel full — not acking, JetStream will redeliver"
            );
            MessageAction::Continue
        }
        Err(IngestError::ChannelClosed) => MessageAction::FatalChannelClosed,
    }
}
