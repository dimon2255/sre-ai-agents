use async_trait::async_trait;

use crate::notification::Notification;

/// Failure modes when delivering a notification.
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error("notification HTTP error: {0}")]
    Http(String),
    #[error("notification provider rejected delivery: {status} {message}")]
    Provider { status: u16, message: String },
}

/// A delivery destination for platform notifications.
///
/// Implementations MUST be best-effort: return an error rather than panic.
/// The [`crate::NotificationEngine`] logs failures and continues so one
/// broken destination never blocks the others.
#[async_trait]
pub trait Notifier: Send + Sync {
    /// Provider name, for logging (e.g. `"slack"`).
    fn name(&self) -> &str;

    /// Deliver a single notification.
    async fn send(&self, notification: &Notification) -> Result<(), NotifyError>;
}
