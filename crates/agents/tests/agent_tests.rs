use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use sre_azure::auth::MockTokenProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::messages::*;

use sre_agents::traits::SreAgent;
use sre_agents::{LogsAgent, MetricsAgent, TriageAgent};

fn make_alert(resource: &str, severity: AlertSeverity) -> AlertInfo {
    AlertInfo {
        alert_id: "test-alert-001".to_string(),
        alert_rule: "High Error Rate".to_string(),
        severity,
        signal_type: "Metric".to_string(),
        monitor_condition: "Fired".to_string(),
        affected_resource: format!(
            "/subscriptions/abc/resourceGroups/prod-rg/providers/Microsoft.Web/sites/{resource}"
        ),
        fired_at: chrono::Utc::now(),
        description: Some("Test alert".to_string()),
        raw_context: None,
    }
}

/// Helper: send an AgentMessage to an agent with a reply channel, return reply_rx
async fn send_with_reply(
    agent_tx: &mpsc::Sender<InboundMessage>,
    message: AgentMessage,
) -> mpsc::Receiver<AgentMessage> {
    let (reply_tx, reply_rx) = mpsc::channel(16);
    agent_tx
        .send(InboundMessage { message, reply_tx })
        .await
        .unwrap();
    reply_rx
}

// ─── TriageAgent Tests ───

#[tokio::test]
async fn test_triage_agent_classifies_alert() {
    let agent = TriageAgent::new();
    let (agent_tx, agent_rx) = mpsc::channel(16);

    // Spawn agent
    let handle = tokio::spawn(async move { agent.run(agent_rx).await });

    // Send investigation request
    let alert = make_alert("payments-api", AlertSeverity::Sev1);
    let correlation_id = uuid::Uuid::new_v4();
    let msg = AgentMessage::new(
        correlation_id,
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("triage-agent".to_string())),
        AgentPayload::InvestigationRequest(InvestigationContext {
            alert,
            additional_context: None,
        }),
        1.0,
    );

    let mut reply_rx = send_with_reply(&agent_tx, msg).await;

    // Receive triage result
    let response = tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.correlation_id, correlation_id);
    assert!(response.confidence >= 0.9); // Sev1 should have high confidence

    if let AgentPayload::TriageResult(result) = &response.payload {
        assert_eq!(result.affected_service, "payments-api");
        assert!(matches!(result.severity, AlertSeverity::Sev1));
        assert!(!result.recommended_agents.is_empty());
        assert_eq!(result.investigation_params.resource_name, "payments-api");
        assert_eq!(
            result.investigation_params.resource_group,
            Some("prod-rg".to_string())
        );
        assert_eq!(result.investigation_params.timespan_minutes, 30); // Sev1 = 30min
    } else {
        panic!("Expected TriageResult payload");
    }

    // Drop sender to close agent
    drop(agent_tx);
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_triage_agent_lower_severity_longer_timespan() {
    let agent = TriageAgent::new();
    let (agent_tx, agent_rx) = mpsc::channel(16);

    let handle = tokio::spawn(async move { agent.run(agent_rx).await });

    let alert = make_alert("web-frontend", AlertSeverity::Sev3);
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("triage-agent".to_string())),
        AgentPayload::InvestigationRequest(InvestigationContext {
            alert,
            additional_context: None,
        }),
        1.0,
    );

    let mut reply_rx = send_with_reply(&agent_tx, msg).await;

    let response = tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
        .await
        .unwrap()
        .unwrap();

    if let AgentPayload::TriageResult(result) = &response.payload {
        assert_eq!(result.investigation_params.timespan_minutes, 120); // Sev3 = 120min
    } else {
        panic!("Expected TriageResult");
    }

    drop(agent_tx);
    handle.await.unwrap().unwrap();
}

#[tokio::test]
async fn test_triage_agent_health_check() {
    let agent = TriageAgent::new();
    let (agent_tx, agent_rx) = mpsc::channel(16);

    let handle = tokio::spawn(async move { agent.run(agent_rx).await });

    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("triage-agent".to_string())),
        AgentPayload::HealthCheck,
        1.0,
    );

    let mut reply_rx = send_with_reply(&agent_tx, msg).await;

    let response = tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
        .await
        .unwrap()
        .unwrap();

    if let AgentPayload::HealthStatus(health) = &response.payload {
        assert!(matches!(health.status, AgentStatus::Ready));
    } else {
        panic!("Expected HealthStatus");
    }

    drop(agent_tx);
    handle.await.unwrap().unwrap();
}

// ─── LogsAgent Tests ───

#[tokio::test]
async fn test_logs_agent_with_no_workspaces() {
    // LogsAgent with empty workspace list — returns quickly with no findings
    let credential = Arc::new(MockTokenProvider::new("test-token"));
    let agent = LogsAgent::new(Arc::new(AzureQueryClient::new(credential)), vec![], 1000);

    let (agent_tx, agent_rx) = mpsc::channel(16);

    let handle = tokio::spawn(async move { agent.run(agent_rx).await });

    let correlation_id = uuid::Uuid::new_v4();
    let msg = AgentMessage::new(
        correlation_id,
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("logs-agent".to_string())),
        AgentPayload::AnalysisTask(AnalysisTask {
            task_type: AnalysisTaskType::ErrorAnalysis,
            params: InvestigationParams {
                resource_name: "payments-api".to_string(),
                resource_group: Some("prod-rg".to_string()),
                timespan_minutes: 30,
                operation_ids: vec![],
            },
        }),
        1.0,
    );

    let mut reply_rx = send_with_reply(&agent_tx, msg).await;

    let response = tokio::time::timeout(Duration::from_secs(5), reply_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.correlation_id, correlation_id);
    if let AgentPayload::AnalysisResult(result) = &response.payload {
        assert!(!result.summary.is_empty());
        assert_eq!(result.findings.len(), 0); // no workspaces = no findings
        assert_eq!(result.queries_executed.len(), 0);
    } else {
        panic!("Expected AnalysisResult");
    }

    drop(agent_tx);
    handle.await.unwrap().unwrap();
}

// ─── MetricsAgent Tests ───

#[tokio::test]
async fn test_metrics_agent_produces_analysis_result() {
    let credential = Arc::new(MockTokenProvider::new("test-token"));
    let agent = MetricsAgent::new(
        Arc::new(AzureQueryClient::new(credential)),
        vec!["test-app-id".to_string()],
    );

    let (agent_tx, agent_rx) = mpsc::channel(16);

    let handle = tokio::spawn(async move { agent.run(agent_rx).await });

    let correlation_id = uuid::Uuid::new_v4();
    let msg = AgentMessage::new(
        correlation_id,
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("metrics-agent".to_string())),
        AgentPayload::AnalysisTask(AnalysisTask {
            task_type: AnalysisTaskType::PerformanceAnalysis,
            params: InvestigationParams {
                resource_name: "payments-api".to_string(),
                resource_group: Some("prod-rg".to_string()),
                timespan_minutes: 30,
                operation_ids: vec![],
            },
        }),
        1.0,
    );

    let mut reply_rx = send_with_reply(&agent_tx, msg).await;

    let response = tokio::time::timeout(Duration::from_secs(10), reply_rx.recv())
        .await
        .unwrap()
        .unwrap();

    assert_eq!(response.correlation_id, correlation_id);

    if let AgentPayload::AnalysisResult(result) = &response.payload {
        assert!(!result.summary.is_empty());
    } else {
        panic!("Expected AnalysisResult");
    }

    drop(agent_tx);
    handle.await.unwrap().unwrap();
}

// ─── Agent Trait Tests ───

#[test]
fn test_agent_capabilities() {
    let triage = TriageAgent::new();
    assert!(triage
        .capabilities()
        .contains(&"alert_classification".to_string()));

    let credential = Arc::new(MockTokenProvider::new("token"));
    let logs = LogsAgent::new(
        Arc::new(AzureQueryClient::new(credential.clone())),
        vec![],
        100,
    );
    assert!(logs.capabilities().contains(&"error_analysis".to_string()));

    let metrics = MetricsAgent::new(Arc::new(AzureQueryClient::new(credential)), vec![]);
    assert!(metrics
        .capabilities()
        .contains(&"performance_analysis".to_string()));
}

#[test]
fn test_agent_types() {
    let triage = TriageAgent::new();
    assert!(matches!(triage.agent_type(), AgentType::Triage));

    let credential = Arc::new(MockTokenProvider::new("token"));
    let logs = LogsAgent::new(
        Arc::new(AzureQueryClient::new(credential.clone())),
        vec![],
        100,
    );
    assert!(matches!(logs.agent_type(), AgentType::Logs));

    let metrics = MetricsAgent::new(Arc::new(AzureQueryClient::new(credential)), vec![]);
    assert!(matches!(metrics.agent_type(), AgentType::Metrics));
}
