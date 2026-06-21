//! Full-engine integration tests that run the orchestrator's `investigate()`
//! loop end-to-end against an in-memory store — no Docker / Postgres required.
//!
//! The Postgres-backed `investigation_tests.rs` cover the same flow against a
//! real DB; these cover the engine logic (dedup layers, state machine driving,
//! fan-out, synthesis, and the optional Phase-3 reasoning/RAG/learning block)
//! in environments where Docker is unavailable.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use uuid::Uuid;

use sre_agents::traits::SreAgent;
use sre_agents::{AgentHandle, LogsAgent, MetricsAgent, TriageAgent};
use sre_azure::auth::MockTokenProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::PlatformConfig;
use sre_common::errors::StoreError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::*;
use sre_intelligence::{HashEmbedder, LearningLoop, MockKnowledgeStore, MockReasoningEngine};
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::models::{WorkflowRecord, WorkflowState};
use sre_store::{EventLog, WorkflowStore};

// ─── In-memory store implementing both persistence traits ───

#[derive(Default)]
struct MemStore {
    workflows: Mutex<HashMap<Uuid, WorkflowRecord>>,
    events: Mutex<Vec<AgentMessage>>,
}

#[async_trait]
impl WorkflowStore for MemStore {
    async fn create_workflow(&self, workflow: &WorkflowRecord) -> Result<(), StoreError> {
        self.workflows
            .lock()
            .unwrap()
            .insert(workflow.id, workflow.clone());
        Ok(())
    }

    async fn try_create_workflow(&self, workflow: &WorkflowRecord) -> Result<bool, StoreError> {
        let mut wf = self.workflows.lock().unwrap();
        if wf.values().any(|w| w.alert_id == workflow.alert_id) {
            return Ok(false);
        }
        wf.insert(workflow.id, workflow.clone());
        Ok(true)
    }

    async fn update_state(&self, id: &Uuid, state: WorkflowState) -> Result<(), StoreError> {
        if let Some(w) = self.workflows.lock().unwrap().get_mut(id) {
            w.state = state;
        }
        Ok(())
    }

    async fn get_workflow(&self, id: &Uuid) -> Result<Option<WorkflowRecord>, StoreError> {
        Ok(self.workflows.lock().unwrap().get(id).cloned())
    }

    async fn get_workflow_by_alert_id(
        &self,
        alert_id: &str,
    ) -> Result<Option<WorkflowRecord>, StoreError> {
        Ok(self
            .workflows
            .lock()
            .unwrap()
            .values()
            .find(|w| w.alert_id == alert_id)
            .cloned())
    }

    async fn get_active_workflows(&self) -> Result<Vec<WorkflowRecord>, StoreError> {
        Ok(self
            .workflows
            .lock()
            .unwrap()
            .values()
            .filter(|w| !w.state.is_terminal())
            .cloned()
            .collect())
    }

    async fn store_report(
        &self,
        id: &Uuid,
        report: &InvestigationReport,
    ) -> Result<(), StoreError> {
        if let Some(w) = self.workflows.lock().unwrap().get_mut(id) {
            w.report_json = Some(serde_json::to_string(report).unwrap());
        }
        Ok(())
    }
}

#[async_trait]
impl EventLog for MemStore {
    async fn append(&self, message: &AgentMessage) -> Result<(), StoreError> {
        self.events.lock().unwrap().push(message.clone());
        Ok(())
    }

    async fn get_by_correlation_id(
        &self,
        correlation_id: &Uuid,
    ) -> Result<Vec<AgentMessage>, StoreError> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|m| &m.correlation_id == correlation_id)
            .cloned()
            .collect())
    }

    async fn count_by_correlation_id(&self, correlation_id: &Uuid) -> Result<u64, StoreError> {
        Ok(self
            .events
            .lock()
            .unwrap()
            .iter()
            .filter(|m| &m.correlation_id == correlation_id)
            .count() as u64)
    }
}

// ─── Helpers ───

fn make_alert(alert_id: &str, resource: &str, severity: AlertSeverity) -> AlertInfo {
    AlertInfo {
        alert_id: alert_id.to_string(),
        alert_rule: "Test Rule".to_string(),
        severity,
        signal_type: "Metric".to_string(),
        monitor_condition: "Fired".to_string(),
        affected_resource: resource.to_string(),
        fired_at: chrono::Utc::now(),
        description: Some("integration test".to_string()),
        raw_context: None,
    }
}

/// Build a router with the triage + logs + metrics agents registered and
/// running. Returns the router plus the spawned task handles (kept alive by the
/// caller for the duration of the test).
fn agent_router(
    query_client: Arc<AzureQueryClient>,
) -> (AgentRouter, Vec<tokio::task::JoinHandle<()>>) {
    let mut router = AgentRouter::new();
    let mut handles = Vec::new();

    let (tx, rx) = mpsc::channel(64);
    let agent = TriageAgent::new();
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move { agent.run(rx).await.unwrap() }));

    let (tx, rx) = mpsc::channel(64);
    let agent = LogsAgent::new(query_client.clone(), vec![], 1000);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move { agent.run(rx).await.unwrap() }));

    let (tx, rx) = mpsc::channel(64);
    let agent = MetricsAgent::new(query_client, vec![]);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move { agent.run(rx).await.unwrap() }));

    (router, handles)
}

fn query_client() -> Arc<AzureQueryClient> {
    let credential: Arc<dyn sre_azure::TokenProvider> = Arc::new(MockTokenProvider::new("test-token"));
    Arc::new(AzureQueryClient::new(credential))
}

// ─── Tests ───

#[tokio::test]
async fn full_investigation_escalates_with_no_findings() {
    let store = Arc::new(MemStore::default());
    let (router, _handles) = agent_router(query_client());
    let orch = Orchestrator::new(
        PlatformConfig::default(),
        router,
        store.clone(),
        store.clone(),
    );

    let alert = make_alert("alert-1", "payments-api", AlertSeverity::Sev1);
    let report = orch.investigate(alert).await.unwrap();

    assert_eq!(report.affected_resource, "payments-api");
    assert!(report.agents_invoked.contains(&"triage-agent".to_string()));
    // No Azure workspaces => agents produce 0 findings => escalate
    assert_eq!(report.overall_confidence, 0.0);
    assert!(report.escalated);
    assert!(!report.summary.is_empty());
    assert!(report.completed_at >= report.started_at);

    // Workflow persisted in a terminal (Escalated) state with the report stored.
    let wid = Uuid::parse_str(&report.investigation_id).unwrap();
    let wf = store.get_workflow(&wid).await.unwrap().unwrap();
    assert_eq!(wf.state, WorkflowState::Escalated);
    assert!(wf.state.is_terminal());
    assert!(wf.report_json.is_some());

    // Audit trail captured, lookups resolve, and no workflow is left active.
    assert!(store.count_by_correlation_id(&wid).await.unwrap() > 0);
    assert!(!store.get_by_correlation_id(&wid).await.unwrap().is_empty());
    assert!(store
        .get_workflow_by_alert_id("alert-1")
        .await
        .unwrap()
        .is_some());
    assert!(store.get_active_workflows().await.unwrap().is_empty());
}

#[tokio::test]
async fn duplicate_alert_skipped_by_in_memory_lru() {
    let store = Arc::new(MemStore::default());
    let (router, _handles) = agent_router(query_client());
    let orch = Orchestrator::new(
        PlatformConfig::default(),
        router,
        store.clone(),
        store.clone(),
    );

    let alert = make_alert("dup-alert", "svc", AlertSeverity::Sev2);
    let first = orch.investigate(alert.clone()).await.unwrap();
    let second = orch.investigate(alert).await.unwrap();

    assert!(!first.investigation_id.starts_with("dup-"));
    // Second pass hits the in-memory LRU and is skipped without dispatch.
    assert!(second.investigation_id.starts_with("dup-"));
    assert!(second.summary.contains("Duplicate alert"));
    assert!(second.agents_invoked.is_empty());
}

#[tokio::test]
async fn duplicate_alert_skipped_by_db_unique_constraint() {
    let store = Arc::new(MemStore::default());

    // Pre-seed a workflow that already owns this alert_id (simulating another
    // replica having claimed it). A *fresh* orchestrator has an empty LRU, so
    // dedup layer 1 passes and layer 2 (try_create_workflow => Ok(false)) fires.
    let existing = WorkflowRecord {
        id: Uuid::new_v4(),
        state: WorkflowState::Resolved,
        alert_id: "claimed-alert".to_string(),
        affected_resource: "svc".to_string(),
        severity: AlertSeverity::Sev2,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        report_json: None,
    };
    store.create_workflow(&existing).await.unwrap();

    let (router, _handles) = agent_router(query_client());
    let orch = Orchestrator::new(
        PlatformConfig::default(),
        router,
        store.clone(),
        store.clone(),
    );

    let alert = make_alert("claimed-alert", "svc", AlertSeverity::Sev2);
    let report = orch.investigate(alert).await.unwrap();

    assert!(report.investigation_id.starts_with("dup-"));
    assert!(report.summary.contains("Duplicate alert"));
}

#[tokio::test]
async fn missing_triage_agent_fails_investigation() {
    let store = Arc::new(MemStore::default());
    // Empty router — routing the triage task fails immediately.
    let orch = Orchestrator::new(
        PlatformConfig::default(),
        AgentRouter::new(),
        store.clone(),
        store.clone(),
    );

    let alert = make_alert("no-triage", "svc", AlertSeverity::Sev1);
    let report = orch.investigate(alert).await.unwrap();

    assert!(!report.escalated);
    let wid = Uuid::parse_str(&report.investigation_id).unwrap();
    let wf = store.get_workflow(&wid).await.unwrap().unwrap();
    assert_eq!(wf.state, WorkflowState::Failed);
}

#[tokio::test]
async fn investigation_with_reasoning_enriches_summary() {
    let store = Arc::new(MemStore::default());
    let (router, _handles) = agent_router(query_client());

    let embedder = Arc::new(HashEmbedder::default());
    let knowledge: Arc<dyn sre_intelligence::KnowledgeStore> =
        Arc::new(MockKnowledgeStore::new(embedder));
    let learning = Arc::new(LearningLoop::new(knowledge.clone()));

    let orch = Orchestrator::new(
        PlatformConfig::default(),
        router,
        store.clone(),
        store.clone(),
    )
    .with_reasoning(Arc::new(MockReasoningEngine::new()))
    .with_knowledge(knowledge.clone())
    .with_learning(learning);

    let alert = make_alert("reason-1", "payments-api", AlertSeverity::Sev1);
    let report = orch.investigate(alert).await.unwrap();

    // The mock reasoning engine prepends a structured RCA header to the summary.
    assert!(
        report.summary.contains("[RCA · mock]"),
        "summary should carry the synthesized RCA header: {}",
        report.summary
    );
}
