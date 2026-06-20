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

/// TraceAgent — correlates distributed traces from Azure Application Insights.
///
/// Joins failed/slow `AppRequests` to the `AppDependencies` calls that share
/// their `OperationId`, localizing which downstream hop in a multi-service
/// trace is responsible for end-to-end failures or latency. This is the
/// "where in the call chain did it break?" agent.
pub struct TraceAgent {
    id: AgentId,
    query_client: Arc<AzureQueryClient>,
    app_insights_ids: Vec<String>,
    default_max_rows: u32,
}

impl TraceAgent {
    pub fn new(
        query_client: Arc<AzureQueryClient>,
        app_insights_ids: Vec<String>,
        default_max_rows: u32,
    ) -> Self {
        Self {
            id: AgentId("trace-agent".to_string()),
            query_client,
            app_insights_ids,
            default_max_rows,
        }
    }

    /// Run distributed-trace correlation for a given resource
    #[tracing::instrument(skip(self), fields(agent.type = "trace", resource = %params.resource_name, app_insights_count = self.app_insights_ids.len()))]
    async fn analyze_traces(
        &self,
        params: &InvestigationParams,
    ) -> (Vec<Finding>, Vec<QueryRecord>) {
        let mut findings = Vec::new();
        let mut queries = Vec::new();

        let timespan = kql_templates::format_api_timespan(params.timespan_minutes);

        for app_id in &self.app_insights_ids {
            // Query 1: Failure correlation across the trace (request -> dependency)
            let kql = kql_templates::trace_failure_correlation(
                &params.resource_name,
                params.timespan_minutes,
                self.default_max_rows,
            );

            let start = std::time::Instant::now();
            match self
                .query_client
                .query_app_insights(app_id, &kql, &timespan)
                .await
            {
                Ok(response) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let row_count: u32 = response.tables.iter().map(|t| t.row_count() as u32).sum();

                    queries.push(QueryRecord {
                        template_name: "trace_failure_correlation".to_string(),
                        parameters: serde_json::json!({
                            "resource_name": params.resource_name,
                            "timespan_minutes": params.timespan_minutes,
                            "app_id": app_id,
                        }),
                        rows_returned: row_count,
                        duration_ms,
                    });

                    if let Some(table) = response.tables.first() {
                        let records = table.to_records();
                        if !records.is_empty() {
                            if let Some(finding) =
                                self.analyze_failure_correlation(&records, &params.resource_name)
                            {
                                findings.push(finding);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(app_id, error = %e, "Trace failure correlation query failed");
                    findings.push(Finding {
                        category: FindingCategory::Informational,
                        severity: FindingSeverity::Low,
                        title: "Trace query failed".to_string(),
                        description: format!(
                            "Failed to correlate traces for App Insights {app_id}: {e}"
                        ),
                        confidence: 1.0,
                        observed_at: None,
                        source_agent: self.id.to_string(),
                        evidence: vec![],
                        suggestions: vec![
                            "Verify App Insights application ID is correct".to_string(),
                            "Check RBAC permissions (Monitoring Reader)".to_string(),
                        ],
                    });
                }
            }

            // Query 2: Latency breakdown — where does end-to-end time go?
            let kql = kql_templates::trace_latency_breakdown(
                &params.resource_name,
                params.timespan_minutes,
                self.default_max_rows,
            );

            let start = std::time::Instant::now();
            match self
                .query_client
                .query_app_insights(app_id, &kql, &timespan)
                .await
            {
                Ok(response) => {
                    let duration_ms = start.elapsed().as_millis() as u64;
                    let row_count: u32 = response.tables.iter().map(|t| t.row_count() as u32).sum();

                    queries.push(QueryRecord {
                        template_name: "trace_latency_breakdown".to_string(),
                        parameters: serde_json::json!({
                            "resource_name": params.resource_name,
                            "timespan_minutes": params.timespan_minutes,
                            "app_id": app_id,
                        }),
                        rows_returned: row_count,
                        duration_ms,
                    });

                    if let Some(table) = response.tables.first() {
                        let records = table.to_records();
                        if !records.is_empty() {
                            if let Some(finding) =
                                self.analyze_latency_breakdown(&records, &params.resource_name)
                            {
                                findings.push(finding);
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(app_id, error = %e, "Trace latency breakdown query failed");
                }
            }
        }

        (findings, queries)
    }

    /// Localize failures to a downstream dependency within failed traces.
    /// Only produces a finding when a failed dependency is implicated.
    fn analyze_failure_correlation(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Option<Finding> {
        let dep_failures: Vec<&serde_json::Map<String, serde_json::Value>> = records
            .iter()
            .filter(|r| {
                r.get("DependencyTarget")
                    .and_then(|v| v.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            })
            .collect();

        if dep_failures.is_empty() {
            return None;
        }

        let total_failed_traces: i64 = records
            .iter()
            .filter_map(|r| r.get("FailedTraces").and_then(|v| v.as_i64()))
            .sum();

        let details: Vec<String> = dep_failures
            .iter()
            .take(5)
            .map(|r| {
                let req = r
                    .get("RequestName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let target = r
                    .get("DependencyTarget")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let dep_type = r
                    .get("DependencyType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let code = r
                    .get("DependencyResultCode")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let traces = r.get("FailedTraces").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("{req} → {target} ({dep_type}, code {code}): {traces} failed traces")
            })
            .collect();

        Some(Finding {
            category: FindingCategory::DependencyFailure,
            severity: if total_failed_traces > 50 {
                FindingSeverity::High
            } else {
                FindingSeverity::Medium
            },
            title: format!(
                "Failed traces localized to downstream dependencies for {resource_name}"
            ),
            description: format!(
                "{} request operation(s) fail with a correlated downstream dependency \
                 failure in the same trace:\n  {}",
                dep_failures.len(),
                details.join("\n  ")
            ),
            confidence: 0.85,
            observed_at: None,
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::QueryResult,
                label: "Trace failure correlation".to_string(),
                data: serde_json::json!({
                    "total_failed_traces": total_failed_traces,
                    "implicated_dependencies": details,
                }),
            }],
            suggestions: vec![
                "Investigate the implicated downstream dependency, not just this service"
                    .to_string(),
                "Check the dependency's own health, error logs, and recent changes".to_string(),
                "Review timeout/retry configuration on the calling side".to_string(),
            ],
        })
    }

    /// Attribute end-to-end latency to downstream dependency time.
    /// Only produces a finding when dependency time dominates (>50% share).
    fn analyze_latency_breakdown(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Option<Finding> {
        let dep_bound: Vec<&serde_json::Map<String, serde_json::Value>> = records
            .iter()
            .filter(|r| {
                r.get("AvgDependencyShare")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0)
                    > 50.0
            })
            .collect();

        if dep_bound.is_empty() {
            return None;
        }

        let details: Vec<String> = dep_bound
            .iter()
            .take(5)
            .map(|r| {
                let op = r
                    .get("OperationName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let total = r
                    .get("AvgRequestMs")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let share = r
                    .get("AvgDependencyShare")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                format!("{op}: {total:.0}ms avg end-to-end, {share:.0}% spent downstream")
            })
            .collect();

        Some(Finding {
            category: FindingCategory::LatencyDegradation,
            severity: FindingSeverity::Medium,
            title: format!("Latency dominated by downstream dependencies for {resource_name}"),
            description: format!(
                "{} slow operation(s) spend the majority of their time in downstream \
                 dependency calls rather than in-process:\n  {}",
                dep_bound.len(),
                details.join("\n  ")
            ),
            confidence: 0.8,
            observed_at: None,
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::QueryResult,
                label: "Trace latency breakdown".to_string(),
                data: serde_json::json!({ "dependency_bound_operations": details }),
            }],
            suggestions: vec![
                "Optimize or parallelize the dominant downstream calls".to_string(),
                "Check the implicated dependency's latency percentiles".to_string(),
                "Consider caching or a circuit breaker for the slow dependency".to_string(),
            ],
        })
    }
}

#[async_trait]
impl SreAgent for TraceAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Trace
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "trace_correlation".to_string(),
            "dependency_localization".to_string(),
            "latency_attribution".to_string(),
        ]
    }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        info!(agent_id = %self.id, "TraceAgent started");

        while let Some(inbound) = inbox.recv().await {
            let message = &inbound.message;
            let reply_tx = &inbound.reply_tx;

            debug!(
                message_id = %message.id,
                correlation_id = %message.correlation_id,
                "TraceAgent received message"
            );

            match &message.payload {
                AgentPayload::AnalysisTask(task) => {
                    let (findings, queries) = self.analyze_traces(&task.params).await;

                    let overall_confidence = if findings.is_empty() {
                        0.5
                    } else {
                        findings.iter().map(|f| f.confidence).sum::<f64>() / findings.len() as f64
                    };

                    let summary = if findings.is_empty() {
                        format!(
                            "No cross-service trace anomalies found for {}",
                            task.params.resource_name
                        )
                    } else {
                        format!(
                            "Found {} trace findings for {}",
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
                        warn!("TraceAgent: reply channel closed");
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
                    warn!(agent_id = %self.id, "TraceAgent received unexpected payload type");
                }
            }
        }

        info!(agent_id = %self.id, "TraceAgent stopped");
        Ok(())
    }
}
