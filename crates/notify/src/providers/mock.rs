use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tracing::info;

use crate::notification::Notification;
use crate::traits::{Notifier, NotifyError};

/// In-memory notifier: logs every notification and records it for assertions.
/// Used in dev when `notifications.enabled` is true but no real provider is
/// configured, and throughout the test suite.
#[derive(Clone, Default)]
pub struct MockNotifier {
    sent: Arc<Mutex<Vec<Notification>>>,
}

impl MockNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Snapshot of everything sent so far.
    pub fn sent(&self) -> Vec<Notification> {
        self.sent.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Number of notifications recorded.
    pub fn count(&self) -> usize {
        self.sent.lock().map(|v| v.len()).unwrap_or(0)
    }
}

#[async_trait]
impl Notifier for MockNotifier {
    fn name(&self) -> &str {
        "mock"
    }

    async fn send(&self, notification: &Notification) -> Result<(), NotifyError> {
        info!(
            target: "sre_notify::mock",
            investigation_id = %notification.investigation_id,
            severity = %notification.severity,
            title = %notification.title,
            "notification (mock)"
        );
        self.sent
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(notification.clone());
        Ok(())
    }
}
