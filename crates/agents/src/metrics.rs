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

/// MetricsAgent — analyzes performance metrics from Azure Application Insights.
///
/// Uses KQL templates to query AppRequests, AppPerformanceCounters,
/// and AppDependencies tables. Identifies latency degradation,
/// failure rate spikes, and resource exhaustion patterns.
pub struct MetricsAgent {
    id: AgentId,
    query_client: Arc<AzureQueryClient>,
    app_insights_ids: Vec<String>,
}

impl MetricsAgent {
    pub fn new(query_client: Arc<AzureQueryClient>, app_insights_ids: Vec<String>) -> Self {
        Self {
            id: AgentId("metrics-agent".to_string()),
            query_client,
            app_insights_ids,
        }
    }

    /// Run performance analysis for a given resource
    #[tracing::instrument(skip(self), fields(agent.type = "metrics", resource = %params.resource_name, app_insights_count = self.app_insights_ids.len()))]
    async fn analyze_performance(
        &self,
        params: &InvestigationParams,
    ) -> (Vec<Finding>, Vec<QueryRecord>) {
        let mut findings = Vec::new();
        let mut queries = Vec::new();

        let timespan = kql_templates::format_api_timespan(params.timespan_minutes);

        for app_id in &self.app_insights_ids {
            // Query 1: Latency analysis
            let kql =
                kql_templates::latency_analysis(&params.resource_name, params.timespan_minutes);

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
                        template_name: "latency_analysis".to_string(),
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
                            findings.push(
                                self.analyze_latency_results(&records, &params.resource_name)
                                    .with_source_scope(app_id),
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(app_id, error = %e, "Latency analysis query failed");
                    findings.push(
                        Finding {
                            category: FindingCategory::Informational,
                            severity: FindingSeverity::Low,
                            title: "Metrics query failed".to_string(),
                            description: format!(
                                "Failed to query metrics for App Insights {app_id}: {e}"
                            ),
                            confidence: 0.0, // failed query = absence of evidence; keeps escalation honest
                            observed_at: None,
                            source_agent: self.id.to_string(),
                            evidence: vec![],
                            suggestions: vec![
                                "Verify App Insights application ID is correct".to_string(),
                                "Check RBAC permissions (Monitoring Reader)".to_string(),
                            ],
                        }
                        .with_source_scope(app_id),
                    );
                }
            }

            // Query 2: Failure breakdown
            let kql =
                kql_templates::failure_breakdown(&params.resource_name, params.timespan_minutes);

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
                        template_name: "failure_breakdown".to_string(),
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
                            findings.push(
                                self.analyze_failure_results(&records, &params.resource_name)
                                    .with_source_scope(app_id),
                            );
                        }
                    }
                }
                Err(e) => {
                    warn!(app_id, error = %e, "Failure breakdown query failed");
                }
            }

            // Query 3: Dependency performance
            let kql = kql_templates::dependency_performance(
                &params.resource_name,
                params.timespan_minutes,
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
                        template_name: "dependency_performance".to_string(),
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
                                self.analyze_dependency_results(&records, &params.resource_name)
                            {
                                findings.push(finding.with_source_scope(app_id));
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(app_id, error = %e, "Dependency performance query failed");
                }
            }
        }

        (findings, queries)
    }

    /// Analyze latency query results
    fn analyze_latency_results(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Finding {
        // Get the most recent data point
        let latest = &records[0];

        let p95 = latest.get("P95").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let p99 = latest.get("P99").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let failure_rate = latest
            .get("FailureRate")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        let total_requests = latest
            .get("TotalRequests")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);

        let severity = if p95 > 5000.0 || failure_rate > 10.0 {
            FindingSeverity::Critical
        } else if p95 > 2000.0 || failure_rate > 5.0 {
            FindingSeverity::High
        } else if p95 > 1000.0 || failure_rate > 1.0 {
            FindingSeverity::Medium
        } else {
            FindingSeverity::Low
        };

        let confidence = if total_requests > 100 { 0.9 } else { 0.6 };

        Finding {
            category: if failure_rate > 5.0 {
                FindingCategory::ErrorSpike
            } else {
                FindingCategory::LatencyDegradation
            },
            severity,
            title: format!("Performance analysis for {resource_name}"),
            description: format!(
                "P95 latency: {p95:.0}ms, P99: {p99:.0}ms, Failure rate: {failure_rate:.1}%, \
                 Total requests: {total_requests} (most recent minute)"
            ),
            confidence,
            observed_at: latest
                .get("TimeGenerated")
                .and_then(|v| v.as_str())
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|dt| dt.with_timezone(&chrono::Utc)),
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::Metric,
                label: "Latency timeseries".to_string(),
                data: serde_json::json!({
                    "p95_ms": p95,
                    "p99_ms": p99,
                    "failure_rate_pct": failure_rate,
                    "total_requests": total_requests,
                    "data_points": records.len(),
                }),
            }],
            suggestions: if failure_rate > 5.0 {
                vec![
                    "Investigate failure responses — check failure_breakdown for details"
                        .to_string(),
                    "Review application logs for corresponding errors".to_string(),
                ]
            } else if p95 > 2000.0 {
                vec![
                    "Check downstream dependency latency".to_string(),
                    "Review recent deployments for performance regressions".to_string(),
                    "Check resource utilization (CPU, memory)".to_string(),
                ]
            } else {
                vec![]
            },
        }
    }

    /// Analyze failure breakdown results
    fn analyze_failure_results(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Finding {
        let top_failures: Vec<String> = records
            .iter()
            .take(5)
            .map(|r| {
                let op = r
                    .get("OperationName")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let code = r.get("ResultCode").and_then(|v| v.as_str()).unwrap_or("?");
                let count = r.get("FailureCount").and_then(|v| v.as_i64()).unwrap_or(0);
                format!("{op} ({code}): {count} failures")
            })
            .collect();

        let total_failures: i64 = records
            .iter()
            .filter_map(|r| r.get("FailureCount").and_then(|v| v.as_i64()))
            .sum();

        Finding {
            category: FindingCategory::ErrorSpike,
            severity: if total_failures > 50 {
                FindingSeverity::High
            } else {
                FindingSeverity::Medium
            },
            title: format!("Request failures for {resource_name}"),
            description: format!(
                "{total_failures} total failures across {} operations:\n{}",
                records.len(),
                top_failures.join("\n  ")
            ),
            confidence: 0.85,
            observed_at: None,
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::QueryResult,
                label: "Failure breakdown".to_string(),
                data: serde_json::json!({
                    "total_failures": total_failures,
                    "top_operations": top_failures,
                }),
            }],
            suggestions: vec![
                "Correlate with error logs for stack traces".to_string(),
                "Check if failures are concentrated on specific endpoints".to_string(),
            ],
        }
    }

    /// Analyze dependency performance results — only produces a finding if there are issues
    fn analyze_dependency_results(
        &self,
        records: &[serde_json::Map<String, serde_json::Value>],
        resource_name: &str,
    ) -> Option<Finding> {
        // Find dependencies with high failure rates
        let failing_deps: Vec<&serde_json::Map<String, serde_json::Value>> = records
            .iter()
            .filter(|r| r.get("FailureRate").and_then(|v| v.as_f64()).unwrap_or(0.0) > 5.0)
            .collect();

        if failing_deps.is_empty() {
            return None;
        }

        let dep_details: Vec<String> = failing_deps
            .iter()
            .take(5)
            .map(|r| {
                let target = r
                    .get("Target")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let dep_type = r
                    .get("DependencyType")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?");
                let rate = r.get("FailureRate").and_then(|v| v.as_f64()).unwrap_or(0.0);
                format!("{target} ({dep_type}): {rate:.1}% failure rate")
            })
            .collect();

        Some(Finding {
            category: FindingCategory::DependencyFailure,
            severity: FindingSeverity::High,
            title: format!("Dependency failures affecting {resource_name}"),
            description: format!(
                "{} dependencies with >5% failure rate:\n  {}",
                failing_deps.len(),
                dep_details.join("\n  ")
            ),
            confidence: 0.85,
            observed_at: None,
            source_agent: self.id.to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::QueryResult,
                label: "Failing dependencies".to_string(),
                data: serde_json::json!({ "failing_dependencies": dep_details }),
            }],
            suggestions: vec![
                "Check health of identified downstream dependencies".to_string(),
                "Review dependency timeout and retry configurations".to_string(),
                "Check for recent changes in dependency services".to_string(),
            ],
        })
    }
}

#[async_trait]
impl SreAgent for MetricsAgent {
    fn id(&self) -> &AgentId {
        &self.id
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Metrics
    }

    fn capabilities(&self) -> Vec<String> {
        vec![
            "performance_analysis".to_string(),
            "resource_utilization".to_string(),
            "dependency_analysis".to_string(),
        ]
    }

    async fn run(&self, mut inbox: mpsc::Receiver<InboundMessage>) -> Result<(), AgentError> {
        info!(agent_id = %self.id, "MetricsAgent started");

        while let Some(inbound) = inbox.recv().await {
            let message = &inbound.message;
            let reply_tx = &inbound.reply_tx;

            debug!(
                message_id = %message.id,
                correlation_id = %message.correlation_id,
                "MetricsAgent received message"
            );

            match &message.payload {
                AgentPayload::AnalysisTask(task) => {
                    let (findings, queries) = self.analyze_performance(&task.params).await;

                    let overall_confidence = if findings.is_empty() {
                        0.5
                    } else {
                        findings.iter().map(|f| f.confidence).sum::<f64>() / findings.len() as f64
                    };

                    let scope_note = if self.app_insights_ids.len() > 1 {
                        format!(
                            " across {} App Insights applications",
                            self.app_insights_ids.len()
                        )
                    } else {
                        String::new()
                    };
                    let summary = if findings.is_empty() {
                        format!(
                            "No significant metric anomalies found for {}{scope_note}",
                            task.params.resource_name
                        )
                    } else {
                        format!(
                            "Found {} metric findings for {}{scope_note}",
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
                        warn!("MetricsAgent: reply channel closed for this investigation");
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
                    warn!(agent_id = %self.id, "MetricsAgent received unexpected payload type");
                }
            }
        }

        info!(agent_id = %self.id, "MetricsAgent stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::MetricsAgent;
    use serde_json::json;
    use sre_azure::auth::MockTokenProvider;
    use sre_azure::query_client::AzureQueryClient;
    use sre_common::findings::{FindingCategory, FindingSeverity};
    use std::sync::Arc;

    fn agent() -> MetricsAgent {
        let cred = Arc::new(MockTokenProvider::new("test-token"));
        MetricsAgent::new(
            Arc::new(AzureQueryClient::new(cred)),
            vec!["app-insights-1".to_string()],
        )
    }

    fn rec(v: serde_json::Value) -> serde_json::Map<String, serde_json::Value> {
        v.as_object().expect("record must be a JSON object").clone()
    }

    // ─── analyze_latency_results ───

    #[test]
    fn latency_critical_when_p95_exceeds_5s() {
        let a = agent();
        let records = vec![rec(json!({
            "P95": 6000.0, "P99": 9000.0, "FailureRate": 1.0, "TotalRequests": 500,
            "TimeGenerated": "2026-06-20T10:00:00Z",
        }))];
        let f = a.analyze_latency_results(&records, "payments-api");
        assert_eq!(f.severity, FindingSeverity::Critical);
        // failure_rate 1.0 (<5) => latency category, not error spike
        assert_eq!(f.category, FindingCategory::LatencyDegradation);
        // >100 requests => high confidence
        assert!((f.confidence - 0.9).abs() < f64::EPSILON);
        assert!(f.observed_at.is_some());
        assert_eq!(f.source_agent, "metrics-agent");
        // p95 > 2000 => 3 performance suggestions
        assert_eq!(f.suggestions.len(), 3);
        assert!(f.description.contains("6000ms"));
        assert_eq!(f.evidence.len(), 1);
    }

    #[test]
    fn latency_critical_and_errorspike_on_high_failure_rate() {
        let a = agent();
        let records = vec![rec(json!({
            "P95": 100.0, "P99": 200.0, "FailureRate": 12.0, "TotalRequests": 50,
        }))];
        let f = a.analyze_latency_results(&records, "svc");
        // failure_rate 12 > 10 => Critical
        assert_eq!(f.severity, FindingSeverity::Critical);
        // failure_rate > 5 => ErrorSpike category
        assert_eq!(f.category, FindingCategory::ErrorSpike);
        // <100 requests => lower confidence
        assert!((f.confidence - 0.6).abs() < f64::EPSILON);
        // failure_rate > 5 => 2 failure-oriented suggestions
        assert_eq!(f.suggestions.len(), 2);
        // no TimeGenerated => observed_at None
        assert!(f.observed_at.is_none());
    }

    #[test]
    fn latency_high_tier() {
        let a = agent();
        let records = vec![rec(json!({
            "P95": 2500.0, "P99": 3000.0, "FailureRate": 0.0, "TotalRequests": 200,
        }))];
        let f = a.analyze_latency_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::High);
        assert_eq!(f.suggestions.len(), 3); // p95 > 2000
    }

    #[test]
    fn latency_medium_tier() {
        let a = agent();
        let records = vec![rec(json!({
            "P95": 1500.0, "P99": 1800.0, "FailureRate": 0.0, "TotalRequests": 10,
        }))];
        let f = a.analyze_latency_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Medium);
        // p95 not > 2000 and failure not > 5 => no suggestions
        assert!(f.suggestions.is_empty());
    }

    #[test]
    fn latency_low_tier_defaults_when_fields_missing() {
        let a = agent();
        // empty record => all fields default to 0 => Low severity
        let records = vec![rec(json!({}))];
        let f = a.analyze_latency_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Low);
        assert_eq!(f.category, FindingCategory::LatencyDegradation);
        assert!((f.confidence - 0.6).abs() < f64::EPSILON);
        assert!(f.suggestions.is_empty());
        assert!(f.observed_at.is_none());
    }

    // ─── analyze_failure_results ───

    #[test]
    fn failure_results_high_when_many_failures() {
        let a = agent();
        let records = vec![
            rec(json!({"OperationName":"GET /pay","ResultCode":"500","FailureCount":40})),
            rec(json!({"OperationName":"POST /charge","ResultCode":"503","FailureCount":20})),
        ];
        let f = a.analyze_failure_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::High); // 60 > 50
        assert_eq!(f.category, FindingCategory::ErrorSpike);
        assert!(f.description.contains("60 total failures"));
        assert!(f.description.contains("GET /pay"));
    }

    #[test]
    fn failure_results_medium_and_caps_top5() {
        let a = agent();
        let records: Vec<_> = (0..8)
            .map(|i| {
                rec(json!({
                    "OperationName": format!("op{i}"),
                    "ResultCode": "500",
                    "FailureCount": 1,
                }))
            })
            .collect();
        let f = a.analyze_failure_results(&records, "svc");
        assert_eq!(f.severity, FindingSeverity::Medium); // 8 <= 50
        assert!(f.description.contains("across 8 operations"));
        // evidence breakdown lists only the top 5
        let top = f.evidence[0]
            .data
            .get("top_operations")
            .and_then(|v| v.as_array())
            .unwrap();
        assert_eq!(top.len(), 5);
    }

    // ─── analyze_dependency_results ───

    #[test]
    fn dependency_results_none_when_no_failing_deps() {
        let a = agent();
        let records = vec![rec(json!({
            "Target":"sql-db","DependencyType":"SQL","FailureRate": 2.0,
        }))];
        assert!(a.analyze_dependency_results(&records, "svc").is_none());
    }

    #[test]
    fn dependency_results_some_when_failing_dep_present() {
        let a = agent();
        let records = vec![
            rec(json!({"Target":"sql-db","DependencyType":"SQL","FailureRate": 25.0})),
            rec(json!({"Target":"cache","DependencyType":"Redis","FailureRate": 1.0})),
        ];
        let f = a
            .analyze_dependency_results(&records, "svc")
            .expect("should produce a finding");
        assert_eq!(f.severity, FindingSeverity::High);
        assert_eq!(f.category, FindingCategory::DependencyFailure);
        assert!(f.description.contains("sql-db"));
        // the healthy dependency is below threshold and excluded
        assert!(!f.description.contains("cache"));
    }
}
