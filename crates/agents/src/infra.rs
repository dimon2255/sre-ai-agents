use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use sre_azure::kql_templates;
use sre_azure::query_client::AzureQueryClient;
use sre_common::errors::AgentError;
use sre_common::findings::*;
use sre_common::messages::*;

use crate::traits::SreAgent;

/// InfraAgent — detects infrastructure changes correlated with an incident.
///
/// Queries the `AzureActivity` table in Log Analytics for two signals:
/// recent administrative operations (deployments, restarts, scale, config
/// writes) and Azure Resource Health transitions (Available → Degraded /
/// Unavailable). "What changed?" and "is the platform itself unhealthy?" are
/// two of the highest-yield root-cause questions for any incident.
pub struct InfraAgent {
    id: AgentId,
    query_client: Arc<AzureQueryClient>,
    workspace_ids: Vec<String>,
    default_max_rows: u32,
}

impl InfraAgent {
    pub fn new(
        query_client: Arc<AzureQueryClient>,
        workspace_ids: Vec<String>,
        default_max_rows: u32,
    ) -> Self {
        Self {
            id: AgentId("infra-agent".to_string()),
            query_client,
            workspace_ids,
            default_max_rows,
        }
    }

    /// Run infrastructure-change + resource-health analysis for a resource
    #[tracing::instrument(skip(self), fields(agent.type = "infra", resource = %params.resource_name, workspace_count = self.workspace_ids.len()))]
    async fn analyze_infra(
        &self,
        params: &InvestigationParams,
    ) -> (Vec<Finding>, Vec<QueryRecord>) {
        let mut findings = Vec::new();
        let mut queries = Vec::new();

        let timespan = kql_templates::format_api_timespan(params.timespan_minutes);

        for workspace_id in &self.workspace_ids {
            // Query 1: Recent administrative changes (the "what changed?" signal)
            let kql = kql_templates::recent_deployments(
                &params.resource_name,
                params.timespan_minutes,
                self.default_max_rows,
            );

            let start = std::time::Instant::now();
            match self
                .query_client
                .query_log_analytics(workspace_id, &kql, &timespan)
                .await
            {
                Ok(response) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let row_count: u32 = response.tables.iter().map(|t| t.row_count() as u32).sum();

                    queries.push(QueryRecord {
                        template_name: "recent_deployments".to_string(),
                        parameters: serde_json::json!({
                            "resource_name": params.resource_name,
                            "timespan_minutes": params.timespan_minutes,
                            "workspace_id": workspace_id,
                        }),
                        rows_returned: row_count,
                        duration_ms,
                    });

                    if let Some(table) = response.tables.first() {
                        let records = table.to_records();
                        if !records.is_empty() {
                            if let Some(finding) =
                                self.analyze_deployments(&records, &params.resource_name)
                            {
                                findings.push(finding);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(workspace_id, error = %e, "Recent deployments query failed");
                    findings.push(Finding {
                        category: FindingCategory::Informational,
                        severity: FindingSeverity::Low,
                        title: "Activity Log query failed".to_string(),
                        description: format!(
                            "Failed to query Azure Activity Log for workspace {workspace_id}: {e}"
                        ),
                        confidence: 1.0,
                        observed_at: None,
                        source_agent: self.id.to_string(),
                        evidence: vec![],
                        suggestions: vec![
                            "Verify Log Analytics workspace ID is correct".to_string(),
                            "Confirm Activity Log diagnostic settings route to this workspace"
                                .to_string(),
                            "Check RBAC permissions (Log Analytics Reader)".to_string(),
                        ],
                    });
                }
            }

            // Query 2: Resource Health transitions
            let kql = kql_templates::resource_health(
                &params.resource_name,
                params.timespan_minutes,
                self.default_max_rows,
            );

            let start = std::time::Instant::now();
            match self
                .query_client
                .query_log_analytics(workspace_id, &kql, &timespan)
                .await
            {
                Ok(response) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let row_count: u32 = response.tables.iter().map(|t| t.row_count() as u32).sum();

                    queries.push(QueryRecord {
                        template_name: "resource_health".to_string(),
                        parameters: serde_json::json!({
                            "resource_name": params.resource_name,
                            "timespan_minutes": params.timespan_minutes,
                            "workspace_id": workspace_id,
                        }),
                        rows_returned: row_count,
                        duration_ms,
                    });

                    if let Some(table) = response.tables.first() {
                        let records = table.to_records();
                        if !records.is_empty() {
                            if let Some(finding) =
                                self.analyze_health(&records, &params.resource_name)
                            {
                                findings.push(finding);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(workspace_id, error = %e, "Resource health query failed");
                }
            }
        }

        (findings, queries)
    }

    /// Produce a deployment-correlation finding from recent administrative changes.
    fn analyze_deployments(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Option<Finding> {
        if records.is_empty() {
            return None;
        }

        let changes: Vec<String> = records
            .iter()
            .take(5)
            .map(|r| {
                let op = r
                    .get("OperationNameValue")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown operation");
                let status = r
                    .get("ActivityStatusValue")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let caller = r
                    .get("Caller")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let when = r
                    .get("TimeGenerated")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                format!("{when}: {op} [{status}] by {caller}")
            })
            .collect();

        // A change landing close to the incident is a strong correlation signal.
        let observed_at = records.first().and_then(|r| {
            r.get("TimeGenerated")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc))
        });

        Some(Finding {
            category: FindingCategory::DeploymentCorrelation,
            severity: FindingSeverity::Medium,
            title: format!("Recent infrastructure changes to {resource_name}"),
            description: format!(
                "{} administrative change(s) found in the analysis window — a leading \
                 root-cause candidate (\"what changed?\"):\n  {}",
                records.len(),
                changes.join("\n  ")
            ),
            confidence: 0.8,
            observed_at,
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::TimeCorrelation,
                label: "Recent Activity Log changes".to_string(),
                data: serde_json::json!({
                    "change_count": records.len(),
                    "recent_changes": changes,
                }),
            }],
            suggestions: vec![
                format!("Compare the incident start time against the most recent change to {resource_name}"),
                "If a deployment correlates, consider rollback as the first remediation".to_string(),
                "Review the change's author and intent before acting".to_string(),
            ],
        })
    }

    /// Produce a service-health finding when Resource Health degraded.
    fn analyze_health(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Option<Finding> {
        // Find transitions into a non-Available state.
        let degraded: Vec<&serde_json::Map<String, serde_json::Value>> = records
            .iter()
            .filter(|r| {
                r.get("CurrentHealth")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.eq_ignore_ascii_case("Available") && !s.is_empty())
                    .unwrap_or(false)
            })
            .collect();

        if degraded.is_empty() {
            return None;
        }

        let any_unavailable = degraded.iter().any(|r| {
            r.get("CurrentHealth")
                .and_then(|v| v.as_str())
                .map(|s| s.eq_ignore_ascii_case("Unavailable"))
                .unwrap_or(false)
        });

        let transitions: Vec<String> = degraded
            .iter()
            .take(5)
            .map(|r| {
                let prev = r
                    .get("PreviousHealth")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let cur = r
                    .get("CurrentHealth")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let cause = r.get("Cause").and_then(|v| v.as_str()).unwrap_or("unknown");
                let when = r
                    .get("TimeGenerated")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                format!("{when}: {prev} → {cur} (cause: {cause})")
            })
            .collect();

        Some(Finding {
            category: FindingCategory::ServiceHealth,
            severity: if any_unavailable {
                FindingSeverity::Critical
            } else {
                FindingSeverity::High
            },
            title: format!("Azure Resource Health degraded for {resource_name}"),
            description: format!(
                "{} resource-health transition(s) away from Available detected:\n  {}",
                degraded.len(),
                transitions.join("\n  ")
            ),
            confidence: 0.9,
            observed_at: degraded.first().and_then(|r| {
                r.get("TimeGenerated")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            }),
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::QueryResult,
                label: "Resource Health transitions".to_string(),
                data: serde_json::json!({ "transitions": transitions }),
            }],
            suggestions: vec![
                "Check the Azure Service Health blade for a platform-side incident".to_string(),
                "Correlate the health transition time with deployments and traffic".to_string(),
            ],
        })
    }
}

#[async_trait]
impl SreAgent for InfraAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Infra
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "change_correlation".to_string(),
            "deployment_detection".to_string(),
            "resource_health_analysis".to_string(),
        ]
    }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        info!(agent_id = %self.id, "InfraAgent started");

        while let Some(inbound) = inbox.recv().await {
            let message = &inbound.message;
            let reply_tx = &inbound.reply_tx;

            debug!(
                message_id = %message.id,
                correlation_id = %message.correlation_id,
                "InfraAgent received message"
            );

            match &message.payload {
                AgentPayload::AnalysisTask(task) => {
                    let (findings, queries) = self.analyze_infra(&task.params).await;

                    let overall_confidence = if findings.is_empty() {
                        0.5
                    } else {
                        findings.iter().map(|f| f.confidence).sum::<f64>() / findings.len() as f64
                    };

                    let summary = if findings.is_empty() {
                        format!(
                            "No recent infrastructure changes or health transitions for {}",
                            task.params.resource_name
                        )
                    } else {
                        format!(
                            "Found {} infrastructure findings for {}",
                            findings.len(),
                            task.params.resource_name
                        )
                    };

                    let response = AgentMessage::new(
                        message.correlation_id,
                        self.id.clone(),
                        MessageTarget::Orchestrator,
                        AgentPayload::AnalysisResult(AnalysisResult {
                            findings,
                            summary,
                            queries_executed: queries,
                        }),
                        overall_confidence,
                    )
                    .with_parent(message.id);

                    if reply_tx.send(response).await.is_err() {
                        warn!("InfraAgent: reply channel closed");
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
                    let _ = reply_tx.send(response).await;
                }
                _ => {
                    warn!(agent_id = %self.id, "InfraAgent received unexpected payload type");
                }
            }
        }

        info!(agent_id = %self.id, "InfraAgent stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::InfraAgent;
    use serde_json::json;
    use sre_azure::auth::MockTokenProvider;
    use sre_azure::query_client::AzureQueryClient;
    use sre_common::findings::{FindingCategory, FindingSeverity};
    use std::sync::Arc;

    fn agent() -> InfraAgent {
        let cred = Arc::new(MockTokenProvider::new("test-token"));
        InfraAgent::new(
            Arc::new(AzureQueryClient::new(cred)),
            vec!["workspace-1".to_string()],
            50,
        )
    }

    fn rec(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().expect("record must be a JSON object").clone()
    }

    // ─── analyze_deployments ───

    #[test]
    fn deployments_none_when_no_changes() {
        let a = agent();
        let records: Vec<serde_json::Map<String, serde_json::Value>> = vec![];
        assert!(a.analyze_deployments(&records, "svc").is_none());
    }

    #[test]
    fn deployments_some_with_recent_change() {
        let a = agent();
        let records = vec![rec(json!({
            "OperationNameValue":"Microsoft.Web/sites/restart/action",
            "ActivityStatusValue":"Succeeded",
            "Caller":"alice@example.com",
            "TimeGenerated":"2026-06-20T10:00:00Z",
        }))];
        let f = a.analyze_deployments(&records, "svc").unwrap();
        assert_eq!(f.severity, FindingSeverity::Medium);
        assert_eq!(f.category, FindingCategory::DeploymentCorrelation);
        assert!(f.observed_at.is_some());
        assert!(f.description.contains("restart"));
        assert!(f.description.contains("alice@example.com"));
    }

    // ─── analyze_health ───

    #[test]
    fn health_none_when_all_available() {
        let a = agent();
        let records = vec![rec(json!({
            "CurrentHealth":"Available","PreviousHealth":"Available",
        }))];
        assert!(a.analyze_health(&records, "svc").is_none());
    }

    #[test]
    fn health_high_when_degraded() {
        let a = agent();
        let records = vec![rec(json!({
            "CurrentHealth":"Degraded","PreviousHealth":"Available",
            "Cause":"PlatformInitiated","TimeGenerated":"2026-06-20T10:00:00Z",
        }))];
        let f = a.analyze_health(&records, "svc").unwrap();
        assert_eq!(f.severity, FindingSeverity::High);
        assert_eq!(f.category, FindingCategory::ServiceHealth);
        assert!(f.observed_at.is_some());
    }

    #[test]
    fn health_critical_when_any_unavailable() {
        let a = agent();
        let records = vec![rec(json!({
            "CurrentHealth":"Unavailable","PreviousHealth":"Degraded",
            "Cause":"Outage","TimeGenerated":"2026-06-20T10:00:00Z",
        }))];
        let f = a.analyze_health(&records, "svc").unwrap();
        assert_eq!(f.severity, FindingSeverity::Critical);
    }
}
