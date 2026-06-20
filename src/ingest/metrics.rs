//! OpenTelemetry metrics for the NATS JetStream ingestor (ADR-013, PR-2).
//!
//! Instruments are backed by the **global** meter. When no `MeterProvider`
//! is installed (observability disabled — the default for local dev and CI),
//! every instrument is a no-op and recording is effectively free. When
//! `main` installs an OTLP `SdkMeterProvider` (observability enabled), the
//! same instruments export to the OTLP endpoint on the periodic reader.
//!
//! Three signals, per ADR-013:
//! - `nats_consumer_pending`   — consumer backlog at last delivery (gauge)
//! - `nats_redeliveries_total` — messages redelivered (delivered > 1) (counter)
//! - `nats_dlq_total`          — poison messages routed to the DLQ (counter)
//!
//! `consumer_pending` and the redelivery signal are both read from the
//! per-message JetStream `Info` (`pending`, `delivered`), so recording adds
//! no extra round-trips to the NATS server.

use opentelemetry::metrics::{Counter, Gauge};
use opentelemetry::KeyValue;

/// Metric instruments for the NATS ingestor.
#[derive(Clone)]
pub struct NatsMetrics {
    redeliveries: Counter<u64>,
    dlq: Counter<u64>,
    pending: Gauge<u64>,
}

impl NatsMetrics {
    /// Build the instrument set from the global meter. Cheap; the
    /// instruments are no-ops until a `MeterProvider` is installed.
    pub fn new() -> Self {
        let meter = opentelemetry::global::meter("sre.nats");
        Self {
            redeliveries: meter
                .u64_counter("nats_redeliveries_total")
                .with_description(
                    "JetStream messages redelivered to the consumer (delivery count > 1)",
                )
                .build(),
            dlq: meter
                .u64_counter("nats_dlq_total")
                .with_description("Poison messages routed to the dead-letter stream")
                .build(),
            pending: meter
                .u64_gauge("nats_consumer_pending")
                .with_description("Messages pending for the durable consumer at last delivery")
                .build(),
        }
    }

    /// Record per-message JetStream delivery telemetry read from the
    /// message `Info`. `delivered` is the 1-based delivery count (1 = first
    /// delivery, >1 = redelivery); `pending` is the consumer backlog the
    /// server reported with this message.
    pub fn record_delivery(&self, delivered: i64, pending: u64) {
        if delivered > 1 {
            self.redeliveries.add(1, &[]);
        }
        self.pending.record(pending, &[]);
    }

    /// Record one message routed to the dead-letter stream, tagged by reason.
    pub fn record_dlq(&self, reason: &'static str) {
        self.dlq.add(1, &[KeyValue::new("reason", reason)]);
    }
}

impl Default for NatsMetrics {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Without a MeterProvider installed, every instrument is a no-op. These
    // calls must not panic — exercising the recording paths that the
    // ingestor hits on every message.
    #[test]
    fn record_paths_are_noop_without_provider() {
        let m = NatsMetrics::new();
        m.record_delivery(1, 0); // first delivery, empty backlog
        m.record_delivery(4, 12); // redelivery, backlog of 12
        m.record_dlq("parse_failure");
    }

    #[test]
    fn metrics_are_cloneable() {
        // The ingestor clones the instrument set across its loops.
        let m = NatsMetrics::new();
        let c = m.clone();
        c.record_delivery(2, 1);
    }
}
