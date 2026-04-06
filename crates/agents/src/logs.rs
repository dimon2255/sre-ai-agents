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
    async fn analyze_errors(
        &self,
        params: &InvestigationParams,
    ) -> (Vec<Finding>, Vec<QueryRecord>) {
        let mut findings = Vec::new();
        let mut queries = Vec::new();

        let timespan = kql_templates::format_api_timespan(params.timespan_minutes);

        for workspace_id in &self.workspace_ids {
            // Query 1: Error spike analysis
            let kql =
                kql_templates::error_spike(&params.resource_name, params.timespan_minutes, self.default_max_rows);

            let start = std::time::Instant::now();
            match self
                .query_client
                .query_log_analytics(workspace_id, &kql, &timespan)
                .await
            {
                Ok(response) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let row_count: u32 = response
                        .tables
                        .iter()
                        .map(|t| t.row_count() as u32)
                        .sum();

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
                            findings.push(self.analyze_error_spike_results(&records, &params.resource_name));
                        }
                    }
                }
                Err(e) => {
                    warn!(workspace_id, error = %e, "Error spike query failed");
                    findings.push(Finding {
                        category: FindingCategory::Informational,
                        severity: FindingSeverity::Low,
                        title: "Log query failed".to_string(),
                        description: format!(
                            "Failed to query error logs for workspace {workspace_id}: {e}"
                        ),
                        confidence: 1.0,
                        observed_at: None,
                        source_agent: self.id.to_string(),
                        evidence: vec![],
                        suggestions: vec![
                            "Verify Log Analytics workspace ID is correct".to_string(),
                            "Check RBAC permissions (Log Analytics Reader)".to_string(),
                        ],
                    });
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
                    let row_count: u32 = response
                        .tables
                        .iter()
                        .map(|t| t.row_count() as u32)
                        .sum();

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
                            findings.push(self.analyze_error_detail_results(&records, &params.resource_name));
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

    async fn run(
        &self,
        mut inbox: mpsc::Receiver<InboundMessage>,
    ) -> Result<(), AgentError> {
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

                    let summary = if findings.is_empty() {
                        format!("No significant log anomalies found for {}", task.params.resource_name)
                    } else {
                        format!(
                            "Found {} findings for {}",
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
                        warn!("LogsAgent: reply channel closed");
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
                    warn!(agent_id = %self.id, "LogsAgent received unexpected payload type");
                }
            }
        }

        info!(agent_id = %self.id, "LogsAgent stopped");
        Ok(())
    }
}
