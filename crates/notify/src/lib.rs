//! Notification engine (Phase 2b).
//!
//! Delivers investigation reports and escalations to chat / paging
//! destinations. The design mirrors the rest of the platform: a `Notifier`
//! trait with a deterministic `MockNotifier` for dev/CI and real Slack /
//! Teams / PagerDuty bindings activated by configuration. The
//! [`NotificationEngine`] decides *whether* a report warrants a notification
//! (severity threshold + escalation) and fans out to every configured
//! provider concurrently, best-effort.

pub mod engine;
pub mod notification;
pub mod providers;
pub mod traits;

pub use engine::NotificationEngine;
pub use notification::Notification;
pub use providers::{MockNotifier, PagerDutyNotifier, SlackNotifier, TeamsNotifier};
pub use traits::{Notifier, NotifyError};
