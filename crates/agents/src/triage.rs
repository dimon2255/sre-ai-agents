use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use sre_common::errors::AgentError;
use sre_common::messages::*;

use crate::traits::SreAgent;

/// TriageAgent — classifies incoming alerts and creates investigation plans.
///
/// Responsibilities:
/// - Parse and normalize alert information
/// - Classify severity (may override alert's own severity)
/// - Identify the affected service/resource
/// - Determine which investigation agents should be invoked
/// - Extract parameters for downstream agents (resource name, timespan, etc.)
pub struct TriageAgent {
    id: AgentId,
}

impl TriageAgent {
    pub fn new() -> Self {
        Self {
            id: AgentId("triage-agent".to_string()),
        }
    }

    /// Classify an alert and produce a triage result
    fn triage_alert(&self, context: &InvestigationContext) -> TriageResult {
        let alert = &context.alert;

        // Extract resource name from ARM resource ID
        let resource_name = extract_resource_name(&alert.affected_resource);
        let resource_group = extract_resource_group(&alert.affected_resource);

        // Determine timespan based on severity
        let timespan_minutes = match alert.severity {
            AlertSeverity::Sev0 | AlertSeverity::Sev1 => 30,
            AlertSeverity::Sev2 => 60,
            AlertSeverity::Sev3 | AlertSeverity::Sev4 => 120,
        };

        // Determine which agents to invoke based on signal type
        let recommended_agents = match alert.signal_type.as_str() {
            "Metric" => vec![AgentType::Metrics, AgentType::Logs],
            "Log" => vec![AgentType::Logs, AgentType::Metrics],
            _ => vec![AgentType::Logs, AgentType::Metrics],
        };

        // Extract operation IDs from raw context if available
        let operation_ids = alert
            .raw_context
            .as_ref()
            .and_then(|ctx| {
                ctx.get("operationId")
                    .and_then(|v| v.as_str())
                    .map(|id| vec![id.to_string()])
            })
            .unwrap_or_default();

        let signal_summary = format!(
            "{} alert on {} ({})",
            alert.signal_type, resource_name, alert.severity
        );

        info!(
            resource = %resource_name,
            severity = %alert.severity,
            signal = %alert.signal_type,
            agents = ?recommended_agents,
            "Triage complete"
        );

        TriageResult {
            severity: alert.severity,
            affected_service: resource_name.clone(),
            signal_summary,
            recommended_agents,
            investigation_params: InvestigationParams {
                resource_name,
                resource_group,
                timespan_minutes,
                operation_ids,
            },
        }
    }
}

#[async_trait]
impl SreAgent for TriageAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Triage
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "alert_classification".to_string(),
            "severity_assessment".to_string(),
            "investigation_planning".to_string(),
        ]
    }

    async fn run(
        &self,
        mut inbox: mpsc::Receiver<AgentMessage>,
        outbox: mpsc::Sender<AgentMessage>,
    ) -> Result<(), AgentError> {
        info!(agent_id = %self.id, "TriageAgent started");

        while let Some(message) = inbox.recv().await {
            debug!(
                message_id = %message.id,
                correlation_id = %message.correlation_id,
                "TriageAgent received message"
            );

            match &message.payload {
                AgentPayload::InvestigationRequest(context) => {
                    let triage_result = self.triage_alert(context);
                    let confidence = match triage_result.severity {
                        AlertSeverity::Sev0 | AlertSeverity::Sev1 => 0.95,
                        AlertSeverity::Sev2 => 0.85,
                        _ => 0.75,
                    };

                    let response = AgentMessage::new(
                        message.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::TriageResult(triage_result),
                        confidence,
                    )
                    .with_parent(message.id);

                    if outbox.send(response).await.is_err() {
                        warn!("TriageAgent: orchestrator channel closed");
                        break;
                    }
                }
                AgentPayload::HealthCheck => {
                    let response = AgentMessage::new(
                        message.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::HealthStatus(AgentHealth {
                            agent_id: self.id.clone(),
                            status: AgentStatus::Ready,
                            active_tasks: 0,
                            uptime_secs: 0,
                        }),
                        1.0,
                    );
                    let _ = outbox.send(response).await;
                }
                other => {
                    warn!(
                        agent_id = %self.id,
                        payload_type = std::any::type_name_of_val(other),
                        "TriageAgent received unexpected payload type"
                    );
                }
            }
        }

        info!(agent_id = %self.id, "TriageAgent stopped");
        Ok(())
    }
}

/// Extract the resource name from an ARM resource ID
/// e.g., "/subscriptions/.../providers/Microsoft.Web/sites/my-app" -> "my-app"
fn extract_resource_name(resource_id: &str) -> String {
    resource_id
        .rsplit('/')
        .next()
        .unwrap_or(resource_id)
        .to_string()
}

/// Extract resource group from an ARM resource ID
fn extract_resource_group(resource_id: &str) -> Option<String> {
    let parts: Vec<&str> = resource_id.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        if part.eq_ignore_ascii_case("resourceGroups") || part.eq_ignore_ascii_case("resourcegroups")
        {
            return parts.get(i + 1).map(|s| s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_resource_name() {
        let arm_id = "/subscriptions/abc/resourceGroups/my-rg/providers/Microsoft.Web/sites/my-app";
        assert_eq!(extract_resource_name(arm_id), "my-app");
    }

    #[test]
    fn test_extract_resource_name_plain() {
        assert_eq!(extract_resource_name("simple-name"), "simple-name");
    }

    #[test]
    fn test_extract_resource_group() {
        let arm_id = "/subscriptions/abc/resourceGroups/my-rg/providers/Microsoft.Web/sites/my-app";
        assert_eq!(extract_resource_group(arm_id), Some("my-rg".to_string()));
    }

    #[test]
    fn test_extract_resource_group_missing() {
        assert_eq!(extract_resource_group("simple-name"), None);
    }
}
