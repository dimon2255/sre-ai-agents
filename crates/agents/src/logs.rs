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

/// LogsAgent — analyzes error logs and patterns from Azure Log Analytics.
///
/// Uses KQL templates to query AppExceptions and AppTraces tables.
/// Identifies error spikes, exception clustering, and log patterns.
pub struct LogsAgent {
    id: AgentId,
    query_client: Arc<AzureQueryClient>,
    workspace_ids: Vec<String>,
    default_max_rows: u32,
}

impl LogsAgent {
    pub fn new(
        query_client: Arc<AzureQueryClient>,
        workspace_ids: Vec<String>,
        default_max_rows: u32,
    ) -> Self {
        Self {
            id: AgentId("logs-agent".to_string()),
            query_client,
            workspace_ids,
            default_max_rows,
        }
    }

    /// Run error analysis for a given resource
    #[tracing::instrument(skip(self), fields(agent.type = "logs", resource = %params.resource_name, workspace_count = self.workspace_ids.len()))]
    async fn analyze_errors(
        &self,
        params: &InvestigationParams,
    ) -> (Vec<Finding>, Vec<QueryRecord>) {
        let mut findings = Vec::new();
        let mut queries = Vec::new();

        let timespan = kql_templates::format_api_timespan(params.timespan_minutes);

        for workspace_id in &self.workspace_ids {
            // Query 1: Error spike analysis
            let kql = kql_templates::error_spike(
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
                        template_name: "error_spike".to_string(),
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
                            findings.push(
                                self.analyze_error_spike_results(&records, &params.resource_name)
                                    .with_source_scope(workspace_id),
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(workspace_id, error = %e, "Error spike query failed");
                    findings.push(
                        Finding {
                            category: FindingCategory::Informational,
                            severity: FindingSeverity::Low,
                            title: "Log query failed".to_string(),
                            description: format!(
                                "Failed to query error logs for workspace {workspace_id}: {e}"
                            ),
                            confidence: 0.0, // failed query = absence of evidence; keeps escalation honest
                            observed_at: None,
                            source_agent: self.id.to_string(),
                            evidence: vec![],
                            suggestions: vec![
                                "Verify Log Analytics workspace ID is correct".to_string(),
                                "Check RBAC permissions (Log Analytics Reader)".to_string(),
                            ],
                        }
                        .with_source_scope(workspace_id),
                    );
                }
            }

            // Query 2: Error details
            let kql =
                kql_templates::error_details(&params.resource_name, params.timespan_minutes, 20);

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
                        template_name: "error_details".to_string(),
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
                            findings.push(
                                self.analyze_error_detail_results(&records, &params.resource_name)
                                    .with_source_scope(workspace_id),
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(workspace_id, error = %e, "Error details query failed");
                }
            }
        }

        (findings, queries)
    }

    /// Analyze error spike query results and produce a finding
    fn analyze_error_spike_results(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Finding {
        let total_errors: i64 = records
            .iter()
            .filter_map(|r| r.get("ErrorCount").and_then(|v| v.as_i64()))
            .sum();

        let distinct_types: Vec<String> = records
            .iter()
            .filter_map(|r| {
                r.get("ExceptionType")
                    .and_then(|v| v.as_str())
                    .map(String::from)
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();

        let severity = if total_errors > 100 {
            FindingSeverity::Critical
        } else if total_errors > 50 {
            FindingSeverity::High
        } else if total_errors > 10 {
            FindingSeverity::Medium
        } else {
            FindingSeverity::Low
        };

        let confidence = if total_errors > 50 { 0.9 } else { 0.7 };

        Finding {
            category: FindingCategory::ErrorSpike,
            severity,
            title: format!("Error spike detected on {resource_name}"),
            description: format!(
                "Found {total_errors} errors across {} exception types in the analysis window. \
                 Types: {}",
                distinct_types.len(),
                distinct_types.join(", ")
            ),
            confidence,
            observed_at: records.first().and_then(|r| {
                r.get("TimeGenerated")
                    .and_then(|v| v.as_str())
                    .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                    .map(|dt| dt.with_timezone(&chrono::Utc))
            }),
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::QueryResult,
                label: "Error spike data".to_string(),
                data: serde_json::json!({
                    "total_errors": total_errors,
                    "distinct_types": distinct_types,
                    "sample_records": &records[..records.len().min(5)],
                }),
            }],
            suggestions: vec![
                format!("Review recent deployments to {resource_name}"),
                "Check application logs for stack traces".to_string(),
                "Verify downstream dependencies are healthy".to_string(),
            ],
        }
    }

    /// Analyze error detail results
    fn analyze_error_detail_results(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Finding {
        let messages: Vec<String> = records
            .iter()
            .filter_map(|r| {
                r.get("InnermostMessage")
                    .or(r.get("OuterMessage"))
                    .and_then(|v| v.as_str())
                    .map(|s| s.chars().take(200).collect())
            })
            .take(5)
            .collect();

        Finding {
            category: FindingCategory::ErrorSpike,
            severity: FindingSeverity::Info,
            title: format!("Error details for {resource_name}"),
            description: format!(
                "Top {} recent exception messages:\n{}",
                messages.len(),
                messages
                    .iter()
                    .enumerate()
                    .map(|(i, m)| format!("  {}. {m}", i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
            confidence: 0.95,
            observed_at: None,
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::LogExcerpt,
                label: "Recent exceptions".to_string(),
                data: serde_json::json!({ "messages": messages }),
            }],
            suggestions: vec![],
        }
    }
}

#[async_trait]
impl SreAgent for LogsAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Logs
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "error_analysis".to_string(),
            "log_pattern_analysis".to_string(),
        ]
    }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        info!(agent_id = %self.id, "LogsAgent started");

        while let Some(inbound) = inbox.recv().await {
            let message = &inbound.message;
            let reply_tx = &inbound.reply_tx;

            debug!(
                message_id = %message.id,
                correlation_id = %message.correlation_id,
                "LogsAgent received message"
            );

            match &message.payload {
                AgentPayload::AnalysisTask(task) => {
                    let (findings, queries) = self.analyze_errors(&task.params).await;

                    let overall_confidence = if findings.is_empty() {
                        0.5
                    } else {
                        findings.iter().map(|f| f.confidence).sum::<f64>() / findings.len() as f64
                    };

                    let scope_note = if self.workspace_ids.len() > 1 {
                        format!(
                            " across {} Log Analytics workspaces",
                            self.workspace_ids.len()
                        )
                    } else {
                        String::new()
                    };
                    let summary = if findings.is_empty() {
                        format!(
                            "No significant log anomalies found for {}{scope_note}",
                            task.params.resource_name
                        )
                    } else {
                        format!(
                            "Found {} findings for {}{scope_note}",
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
                        // The per-investigation reply channel is closed (the
                        // orchestrator already timed out and moved on). This is a
                        // per-request condition, not a shutdown signal — keep
                        // serving future investigations instead of exiting run().
                        warn!("LogsAgent: reply channel closed for this investigation");
                        continue;
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
                    warn!(agent_id = %self.id, "LogsAgent received unexpected payload type");
                }
            }
        }

        info!(agent_id = %self.id, "LogsAgent stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::LogsAgent;
    use serde_json::json;
    use sre_azure::auth::MockTokenProvider;
    use sre_azure::query_client::AzureQueryClient;
    use sre_common::findings::{FindingCategory, FindingSeverity};
    use std::sync::Arc;

    fn agent() -> LogsAgent {
        let cred = Arc::new(MockTokenProvider::new("test-token"));
        LogsAgent::new(
            Arc::new(AzureQueryClient::new(cred)),
            vec!["workspace-1".to_string()],
            100,
        )
    }

    fn rec(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().expect("record must be a JSON object").clone()
    }

    // ─── analyze_error_spike_results ───

    #[test]
    fn error_spike_critical_above_100() {
        let a = agent();
        let records = vec![
            rec(
                json!({"ErrorCount": 80, "ExceptionType": "NullRef", "TimeGenerated":"2026-06-20T10:00:00Z"}),
            ),
            rec(json!({"ErrorCount": 40, "ExceptionType": "Timeout"})),
        ];
        let f = a.analyze_error_spike_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Critical); // 120 > 100
        assert!((f.confidence - 0.9).abs() < f64::EPSILON); // > 50 errors
        assert_eq!(f.category, FindingCategory::ErrorSpike);
        assert!(f.observed_at.is_some());
        let distinct = f.evidence[0]
            .data
            .get("distinct_types")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(distinct.len(), 2);
    }

    #[test]
    fn error_spike_high_tier() {
        let a = agent();
        let records = vec![rec(json!({"ErrorCount": 60, "ExceptionType": "Timeout"}))];
        let f = a.analyze_error_spike_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::High); // 50 < 60 <= 100
        assert!((f.confidence - 0.9).abs() < f64::EPSILON);
        assert!(f.observed_at.is_none()); // no TimeGenerated
    }

    #[test]
    fn error_spike_medium_tier() {
        let a = agent();
        let records = vec![rec(json!({"ErrorCount": 20, "ExceptionType": "Timeout"}))];
        let f = a.analyze_error_spike_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Medium); // 10 < 20 <= 50
        assert!((f.confidence - 0.7).abs() < f64::EPSILON); // <= 50 errors
    }

    #[test]
    fn error_spike_low_tier() {
        let a = agent();
        let records = vec![rec(json!({"ErrorCount": 5, "ExceptionType": "Timeout"}))];
        let f = a.analyze_error_spike_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Low); // <= 10
        assert!((f.confidence - 0.7).abs() < f64::EPSILON);
    }

    // ─── analyze_error_detail_results ───

    #[test]
    fn error_details_truncates_and_prefers_innermost() {
        let a = agent();
        let long = "x".repeat(500);
        let records = vec![
            rec(json!({"InnermostMessage": long})),
            rec(json!({"OuterMessage": "outer only"})), // falls back to OuterMessage
            rec(json!({"InnermostMessage": "inner", "OuterMessage": "outer"})),
        ];
        let f = a.analyze_error_detail_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Info);
        assert!((f.confidence - 0.95).abs() < f64::EPSILON);
        let msgs = f.evidence[0]
            .data
            .get("messages")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(msgs.len(), 3);
        // first message truncated to 200 chars
        assert_eq!(msgs[0].as_str().unwrap().chars().count(), 200);
        // second falls back to OuterMessage
        assert_eq!(msgs[1].as_str().unwrap(), "outer only");
        // third prefers InnermostMessage over OuterMessage
        assert_eq!(msgs[2].as_str().unwrap(), "inner");
    }
}
