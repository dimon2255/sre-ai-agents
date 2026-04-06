use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::timeout;
use tracing::{info, warn};

use sre_common::messages::AgentMessage;

/// Receives a message from an agent channel with a timeout.
///
/// If the agent doesn't respond within the timeout, returns None
/// and logs a warning. The orchestrator can then proceed with
/// partial results (circuit-breaking pattern).
pub async fn recv_with_timeout(
    receiver: &mut mpsc::Receiver<AgentMessage>,
    timeout_duration: Duration,
    context: &str,
) -> Option<AgentMessage> {
    match timeout(timeout_duration, receiver.recv()).await {
        Ok(Some(msg)) => Some(msg),
        Ok(None) => {
            info!(context, "Agent channel closed");
            None
        }
        Err(_) => {
            warn!(
                context,
                timeout_secs = timeout_duration.as_secs(),
                "Agent timed out — proceeding with partial results"
            );
            None
        }
    }
}

/// Collect responses from multiple agent channels with a shared timeout.
///
/// Waits for `expected_count` responses or until timeout expires,
/// whichever comes first. Returns all responses received.
pub async fn collect_with_timeout(
    receiver: &mut mpsc::Receiver<AgentMessage>,
    expected_count: usize,
    timeout_duration: Duration,
) -> Vec<AgentMessage> {
    let mut responses = Vec::with_capacity(expected_count);
    let deadline = tokio::time::Instant::now() + timeout_duration;

    for i in 0..expected_count {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            warn!(
                received = responses.len(),
                expected = expected_count,
                "Timeout expired while waiting for agent responses"
            );
            break;
        }

        match timeout(remaining, receiver.recv()).await {
            Ok(Some(msg)) => {
                responses.push(msg);
            }
            Ok(None) => {
                info!("Agent channel closed after {} responses", responses.len());
                break;
            }
            Err(_) => {
                warn!(
                    received = responses.len(),
                    expected = expected_count,
                    "Timeout waiting for agent response {}/{}",
                    i + 1,
                    expected_count
                );
                break;
            }
        }
    }

    responses
}
