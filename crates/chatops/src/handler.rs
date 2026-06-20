//! ChatOps dispatch: parse → call the backend → format a Slack-style reply.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Serialize;

use sre_common::messages::AlertSeverity;

use crate::command::{parse_command, ChatCommand};

/// A point-in-time view of an investigation, returned by [`ChatBackend::get_status`].
/// Decoupled from the store's `WorkflowRecord` so this crate stays `sre-common`-only.
#[derive(Debug, Clone)]
pub struct InvestigationStatus {
    pub id: String,
    pub state: String,
    pub affected_resource: String,
    pub severity: String,
    /// RCA / summary, present once the investigation has produced a report.
    pub summary: Option<String>,
    pub confidence: Option<f64>,
    pub escalated: bool,
}

/// The operations ChatOps needs from the platform. Implemented by the webhook
/// server over the alert channel (trigger) + the store (status); a mock backs the
/// handler's unit tests.
#[async_trait]
pub trait ChatBackend: Send + Sync {
    /// Queue an investigation for `resource` at `severity`. Returns a reference
    /// the user can pass back to `status` (the alert/correlation id).
    async fn trigger_investigation(
        &self,
        resource: &str,
        severity: AlertSeverity,
    ) -> Result<String, String>;

    /// Look up an investigation's current status by id.
    async fn get_status(
        &self,
        investigation_id: &str,
    ) -> Result<Option<InvestigationStatus>, String>;
}

/// A Slack slash-command response (`response_type` + `text`). `ephemeral` is
/// visible only to the caller; `in_channel` is posted to the whole channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChatResponse {
    pub response_type: String,
    pub text: String,
}

impl ChatResponse {
    pub fn ephemeral(text: impl Into<String>) -> Self {
        Self {
            response_type: "ephemeral".to_string(),
            text: text.into(),
        }
    }

    pub fn in_channel(text: impl Into<String>) -> Self {
        Self {
            response_type: "in_channel".to_string(),
            text: text.into(),
        }
    }
}

/// Parses chat input and dispatches it to a [`ChatBackend`].
pub struct ChatOpsHandler {
    backend: Arc<dyn ChatBackend>,
}

impl ChatOpsHandler {
    pub fn new(backend: Arc<dyn ChatBackend>) -> Self {
        Self { backend }
    }

    /// Handle one chat message, returning the reply to post back.
    pub async fn handle(&self, input: &str) -> ChatResponse {
        match parse_command(input) {
            ChatCommand::Investigate { resource, severity } => {
                match self.backend.trigger_investigation(&resource, severity).await {
                    Ok(reference) => ChatResponse::in_channel(format!(
                        ":mag: Investigation queued for *{resource}* ({}). Ref `{reference}` — use `status {reference}` to check progress.",
                        severity.as_label()
                    )),
                    Err(e) => {
                        ChatResponse::ephemeral(format!(":warning: Couldn't start the investigation: {e}"))
                    }
                }
            }
            ChatCommand::Status { investigation_id } => {
                match self.backend.get_status(&investigation_id).await {
                    Ok(Some(status)) => ChatResponse::ephemeral(format_status(&status)),
                    Ok(None) => ChatResponse::ephemeral(format!(
                        ":question: No investigation found for `{investigation_id}`."
                    )),
                    Err(e) => {
                        ChatResponse::ephemeral(format!(":warning: Status lookup failed: {e}"))
                    }
                }
            }
            ChatCommand::Help => ChatResponse::ephemeral(help_text()),
            ChatCommand::Unknown { raw } => ChatResponse::ephemeral(format!(
                ":grey_question: I didn't understand `{raw}`.\n\n{}",
                help_text()
            )),
        }
    }
}

/// Render an [`InvestigationStatus`] as Slack mrkdwn.
pub fn format_status(s: &InvestigationStatus) -> String {
    let mut out = format!(
        ":clipboard: *{}* — `{}`\nResource: {}  •  Severity: {}",
        s.state, s.id, s.affected_resource, s.severity
    );
    if let Some(c) = s.confidence {
        out.push_str(&format!("  •  Confidence: {:.0}%", c * 100.0));
    }
    if s.escalated {
        out.push_str("  •  :rotating_light: escalated");
    }
    if let Some(summary) = &s.summary {
        out.push_str("\n\n");
        out.push_str(summary);
    }
    out
}

fn help_text() -> String {
    "*SRE ChatOps* — commands:\n\
     • `investigate <resource> [severity]` — start an investigation (severity: sev0–sev4 / critical|high|medium|low|info; default sev3)\n\
     • `status <investigation-id>` — show an investigation's state and RCA\n\
     • `help` — show this message"
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockBackend {
        triggered: Mutex<Vec<(String, AlertSeverity)>>,
        status: Option<InvestigationStatus>,
        fail_trigger: bool,
    }

    #[async_trait]
    impl ChatBackend for MockBackend {
        async fn trigger_investigation(
            &self,
            resource: &str,
            severity: AlertSeverity,
        ) -> Result<String, String> {
            if self.fail_trigger {
                return Err("channel full".to_string());
            }
            self.triggered
                .lock()
                .unwrap()
                .push((resource.to_string(), severity));
            Ok("alert-123".to_string())
        }

        async fn get_status(
            &self,
            _investigation_id: &str,
        ) -> Result<Option<InvestigationStatus>, String> {
            Ok(self.status.clone())
        }
    }

    fn handler(backend: MockBackend) -> ChatOpsHandler {
        ChatOpsHandler::new(Arc::new(backend))
    }

    #[tokio::test]
    async fn investigate_triggers_backend_and_acks_in_channel() {
        let backend = Arc::new(MockBackend::default());
        let h = ChatOpsHandler::new(backend.clone());
        let resp = h.handle("investigate payments-api sev1").await;
        assert_eq!(resp.response_type, "in_channel");
        assert!(resp.text.contains("payments-api"));
        assert!(resp.text.contains("alert-123"));
        let calls = backend.triggered.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], ("payments-api".to_string(), AlertSeverity::Sev1));
    }

    #[tokio::test]
    async fn investigate_failure_is_ephemeral_warning() {
        let h = handler(MockBackend {
            fail_trigger: true,
            ..Default::default()
        });
        let resp = h.handle("investigate api").await;
        assert_eq!(resp.response_type, "ephemeral");
        assert!(resp.text.contains("Couldn't start"));
    }

    #[tokio::test]
    async fn status_found_renders_report() {
        let h = handler(MockBackend {
            status: Some(InvestigationStatus {
                id: "abc".to_string(),
                state: "Resolved".to_string(),
                affected_resource: "payments-api".to_string(),
                severity: "Sev1".to_string(),
                summary: Some("Root cause: deploy regression".to_string()),
                confidence: Some(0.92),
                escalated: false,
            }),
            ..Default::default()
        });
        let resp = h.handle("status abc").await;
        assert!(resp.text.contains("Resolved"));
        assert!(resp.text.contains("92%"));
        assert!(resp.text.contains("deploy regression"));
    }

    #[tokio::test]
    async fn status_not_found_is_helpful() {
        let h = handler(MockBackend::default());
        let resp = h.handle("status nope").await;
        assert!(resp.text.contains("No investigation found"));
    }

    #[tokio::test]
    async fn help_and_unknown_return_usage() {
        let h = handler(MockBackend::default());
        assert!(h.handle("help").await.text.contains("commands"));
        assert!(h
            .handle("frobnicate")
            .await
            .text
            .contains("didn't understand"));
    }
}
