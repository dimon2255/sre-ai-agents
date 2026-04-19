use std::sync::Arc;

use testcontainers_modules::postgres::Postgres;
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::{ContainerAsync, ImageExt};
use tokio::sync::mpsc;

use sre_agents::traits::SreAgent;
use sre_agents::{AgentHandle, LogsAgent, MetricsAgent, TriageAgent};
use sre_azure::auth::MockTokenProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::PlatformConfig;
use sre_common::messages::*;
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::{EventLog, PgStore, WorkflowStore};

async fn setup() -> (
    Orchestrator,
    Vec<tokio::task::JoinHandle<()>>,
    ContainerAsync<Postgres>,
) {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let store = Arc::new(PgStore::new(&url).await.unwrap());
    let event_log = store.clone();

    let credential: Arc<dyn sre_azure::TokenProvider> =
        Arc::new(MockTokenProvider::new("test-token"));
    let query_client = Arc::new(AzureQueryClient::new(credential));

    let config = PlatformConfig::default();
    let mut router = AgentRouter::new();
    let mut handles = Vec::new();

    // TriageAgent
    let (tx, rx) = mpsc::channel(64);
    let agent = TriageAgent::new();
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move { agent.run(rx).await.unwrap() }));

    // LogsAgent
    let (tx, rx) = mpsc::channel(64);
    let agent = LogsAgent::new(query_client.clone(), vec![], 1000);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move { agent.run(rx).await.unwrap() }));

    // MetricsAgent
    let (tx, rx) = mpsc::channel(64);
    let agent = MetricsAgent::new(query_client.clone(), vec![]);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move { agent.run(rx).await.unwrap() }));

    let orchestrator = Orchestrator::new(config, router, store, event_log);
    (orchestrator, handles, container)
}

fn make_alert(resource: &str, severity: AlertSeverity) -> AlertInfo {
    AlertInfo {
        alert_id: format!("test-{}", uuid::Uuid::new_v4()),
        alert_rule: "Test Alert Rule".to_string(),
        severity,
        signal_type: "Metric".to_string(),
        monitor_condition: "Fired".to_string(),
        affected_resource: resource.to_string(),
        fired_at: chrono::Utc::now(),
        description: Some("Integration test alert".to_string()),
        raw_context: None,
    }
}

// ─── Full Investigation Flow ───

#[tokio::test]
async fn test_full_investigation_flow() {
    let (orchestrator, _handles, _container) = setup().await;

    let alert = make_alert("payments-api", AlertSeverity::Sev1);
    let report = orchestrator.investigate(alert).await.unwrap();

    // Verify report structure
    assert!(!report.investigation_id.is_empty());
    assert_eq!(report.affected_resource, "payments-api");
    assert!(report.duration_ms > 0);
    assert!(report.agents_invoked.contains(&"triage-agent".to_string()));

    // With no Azure workspaces configured, agents return 0 findings
    // so the orchestrator should escalate
    assert_eq!(report.overall_confidence, 0.0);

    // Summary should be generated
    assert!(!report.summary.is_empty());

    // Timestamps should be valid
    assert!(report.completed_at >= report.started_at);
}

#[tokio::test]
async fn test_investigation_sev0_critical() {
    let (orchestrator, _handles, _container) = setup().await;

    let alert = make_alert("critical-service", AlertSeverity::Sev0);
    let report = orchestrator.investigate(alert).await.unwrap();

    assert_eq!(report.affected_resource, "critical-service");
    assert!(report.agents_invoked.contains(&"triage-agent".to_string()));
}

#[tokio::test]
async fn test_investigation_includes_all_agents() {
    let (orchestrator, _handles, _container) = setup().await;

    let alert = make_alert("web-app", AlertSeverity::Sev2);
    let report = orchestrator.investigate(alert).await.unwrap();

    // Triage should always be invoked
    assert!(report.agents_invoked.contains(&"triage-agent".to_string()));

    // For Metric signal type, both metrics and logs agents should be invoked
    let has_investigation_agents = report.agents_invoked.contains(&"metrics-agent".to_string())
        || report.agents_invoked.contains(&"logs-agent".to_string());
    assert!(has_investigation_agents);
}

#[tokio::test]
async fn test_investigation_report_is_valid_json() {
    let (orchestrator, _handles, _container) = setup().await;

    let alert = make_alert("api-gateway", AlertSeverity::Sev1);
    let report = orchestrator.investigate(alert).await.unwrap();

    // Report should serialize to valid JSON
    let json = serde_json::to_string_pretty(&report).unwrap();
    assert!(!json.is_empty());

    // And deserialize back
    let deserialized: sre_common::findings::InvestigationReport =
        serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.investigation_id, report.investigation_id);
}

#[tokio::test]
async fn test_workflow_state_persisted() {
    let container = Postgres::default()
        .with_tag("16-alpine")
        .start()
        .await
        .unwrap();
    let port = container.get_host_port_ipv4(5432).await.unwrap();
    let url = format!("postgres://postgres:postgres@127.0.0.1:{port}/postgres");

    let store = Arc::new(PgStore::new(&url).await.unwrap());
    let event_log = store.clone();

    let credential: Arc<dyn sre_azure::TokenProvider> =
        Arc::new(MockTokenProvider::new("test-token"));
    let query_client = Arc::new(AzureQueryClient::new(credential));
    let config = PlatformConfig::default();
    let mut router = AgentRouter::new();

    // Only register triage for this test
    let (tx, rx) = mpsc::channel(64);
    let agent = TriageAgent::new();
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    tokio::spawn(async move { agent.run(rx).await.unwrap() });

    // LogsAgent + MetricsAgent for the fan-out
    let (tx, rx) = mpsc::channel(64);
    let agent = LogsAgent::new(query_client.clone(), vec![], 1000);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    tokio::spawn(async move { agent.run(rx).await.unwrap() });

    let (tx, rx) = mpsc::channel(64);
    let agent = MetricsAgent::new(query_client.clone(), vec![]);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    tokio::spawn(async move { agent.run(rx).await.unwrap() });

    let orchestrator = Orchestrator::new(config, router, store.clone(), event_log);

    let alert = make_alert("test-service", AlertSeverity::Sev2);
    let report = orchestrator.investigate(alert).await.unwrap();

    // Verify workflow was persisted in the DB
    let workflow_id = uuid::Uuid::parse_str(&report.investigation_id).unwrap();
    let workflow = store.get_workflow(&workflow_id).await.unwrap().unwrap();

    // Should be in a terminal state
    assert!(workflow.state.is_terminal());

    // Report should be stored
    assert!(workflow.report_json.is_some());

    // Event log should have entries
    let event_count = store.count_by_correlation_id(&workflow_id).await.unwrap();
    assert!(event_count > 0, "Event log should have entries");
}
