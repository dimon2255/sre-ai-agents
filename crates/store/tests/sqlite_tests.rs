use chrono::Utc;
use tempfile::TempDir;
use uuid::Uuid;

use sre_common::findings::{
    Evidence, EvidenceType, Finding, FindingCategory, FindingSeverity, InvestigationReport,
};
use sre_common::messages::*;
use sre_store::models::{WorkflowRecord, WorkflowState};
use sre_store::traits::{EventLog, WorkflowStore};
use sre_store::SqliteStore;

async fn create_test_store() -> (SqliteStore, TempDir) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let url = format!("sqlite://{}", db_path.display());
    let store = SqliteStore::new(&url).await.unwrap();
    (store, dir) // keep dir alive so temp dir isn't deleted
}

fn make_workflow() -> WorkflowRecord {
    let now = Utc::now();
    WorkflowRecord {
        id: Uuid::new_v4(),
        state: WorkflowState::Created,
        alert_id: "test-alert-123".to_string(),
        affected_resource: "payments-service".to_string(),
        severity: AlertSeverity::Sev1,
        created_at: now,
        updated_at: now,
        report_json: None,
    }
}

fn make_agent_message(correlation_id: Uuid) -> AgentMessage {
    AgentMessage::new(
        correlation_id,
        AgentId("test-agent".to_string()),
        MessageTarget::Orchestrator,
        AgentPayload::HealthCheck,
        0.95,
    )
}

// ─── WorkflowStore Tests ───

#[tokio::test]
async fn test_create_and_get_workflow() {
    let (store, _dir) = create_test_store().await;
    let workflow = make_workflow();
    let id = workflow.id;

    store.create_workflow(&workflow).await.unwrap();

    let retrieved = store.get_workflow(&id).await.unwrap().unwrap();
    assert_eq!(retrieved.id, id);
    assert_eq!(retrieved.alert_id, "test-alert-123");
    assert_eq!(retrieved.affected_resource, "payments-service");
    assert!(matches!(retrieved.state, WorkflowState::Created));
    assert!(matches!(retrieved.severity, AlertSeverity::Sev1));
}

#[tokio::test]
async fn test_get_nonexistent_workflow() {
    let (store, _dir) = create_test_store().await;
    let result = store.get_workflow(&Uuid::new_v4()).await.unwrap();
    assert!(result.is_none());
}

#[tokio::test]
async fn test_update_workflow_state() {
    let (store, _dir) = create_test_store().await;
    let workflow = make_workflow();
    let id = workflow.id;

    store.create_workflow(&workflow).await.unwrap();
    store
        .update_state(&id, WorkflowState::Triaging)
        .await
        .unwrap();

    let retrieved = store.get_workflow(&id).await.unwrap().unwrap();
    assert!(matches!(retrieved.state, WorkflowState::Triaging));
    assert!(retrieved.updated_at > workflow.updated_at);
}

#[tokio::test]
async fn test_update_nonexistent_workflow_fails() {
    let (store, _dir) = create_test_store().await;
    let result = store
        .update_state(&Uuid::new_v4(), WorkflowState::Triaging)
        .await;
    assert!(result.is_err());
}

#[tokio::test]
async fn test_get_active_workflows() {
    let (store, _dir) = create_test_store().await;

    // Create 3 workflows in different states
    let w1 = make_workflow();
    let w2 = make_workflow();
    let w3 = make_workflow();

    store.create_workflow(&w1).await.unwrap();
    store.create_workflow(&w2).await.unwrap();
    store.create_workflow(&w3).await.unwrap();

    // Resolve one, fail another
    store
        .update_state(&w2.id, WorkflowState::Resolved)
        .await
        .unwrap();
    store
        .update_state(&w3.id, WorkflowState::Failed)
        .await
        .unwrap();

    let active = store.get_active_workflows().await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].id, w1.id);
}

#[tokio::test]
async fn test_store_report() {
    let (store, _dir) = create_test_store().await;
    let workflow = make_workflow();
    let id = workflow.id;

    store.create_workflow(&workflow).await.unwrap();

    let report = InvestigationReport {
        investigation_id: id.to_string(),
        started_at: Utc::now(),
        completed_at: Utc::now(),
        duration_ms: 500,
        trigger: "test-alert".to_string(),
        affected_resource: "payments".to_string(),
        overall_confidence: 0.85,
        findings: vec![Finding {
            category: FindingCategory::ErrorSpike,
            severity: FindingSeverity::High,
            title: "Error spike detected".to_string(),
            description: "Found 150 errors".to_string(),
            confidence: 0.9,
            observed_at: None,
            source_agent: "logs-agent".to_string(),
            evidence: vec![Evidence {
                evidence_type: EvidenceType::Metric,
                label: "error_count".to_string(),
                data: serde_json::json!({"count": 150}),
            }],
            suggestions: vec!["Check recent deployments".to_string()],
        }],
        summary: "Error spike on payments".to_string(),
        agents_invoked: vec!["triage-agent".to_string(), "logs-agent".to_string()],
        escalated: false,
        escalation_reason: None,
    };

    store.store_report(&id, &report).await.unwrap();

    let retrieved = store.get_workflow(&id).await.unwrap().unwrap();
    assert!(retrieved.report_json.is_some());

    let stored_report: InvestigationReport =
        serde_json::from_str(retrieved.report_json.as_ref().unwrap()).unwrap();
    assert_eq!(stored_report.findings.len(), 1);
    assert_eq!(stored_report.overall_confidence, 0.85);
    assert_eq!(stored_report.findings[0].title, "Error spike detected");
}

// ─── EventLog Tests ───

#[tokio::test]
async fn test_append_and_retrieve_events() {
    let (store, _dir) = create_test_store().await;
    let correlation_id = Uuid::new_v4();

    let msg1 = make_agent_message(correlation_id);
    let msg2 = make_agent_message(correlation_id);
    let msg3 = make_agent_message(Uuid::new_v4()); // different correlation

    store.append(&msg1).await.unwrap();
    store.append(&msg2).await.unwrap();
    store.append(&msg3).await.unwrap();

    let events = store.get_by_correlation_id(&correlation_id).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].correlation_id, correlation_id);
    assert_eq!(events[1].correlation_id, correlation_id);
}

#[tokio::test]
async fn test_event_count() {
    let (store, _dir) = create_test_store().await;
    let correlation_id = Uuid::new_v4();

    for _ in 0..5 {
        store
            .append(&make_agent_message(correlation_id))
            .await
            .unwrap();
    }

    let count = store
        .count_by_correlation_id(&correlation_id)
        .await
        .unwrap();
    assert_eq!(count, 5);
}

#[tokio::test]
async fn test_event_count_empty() {
    let (store, _dir) = create_test_store().await;
    let count = store
        .count_by_correlation_id(&Uuid::new_v4())
        .await
        .unwrap();
    assert_eq!(count, 0);
}

// ─── WorkflowState Tests ───

#[test]
fn test_valid_state_transitions() {
    assert!(WorkflowState::Created.can_transition_to(&WorkflowState::Triaging));
    assert!(WorkflowState::Created.can_transition_to(&WorkflowState::Failed));
    assert!(WorkflowState::Triaging.can_transition_to(&WorkflowState::Investigating));
    assert!(WorkflowState::Triaging.can_transition_to(&WorkflowState::Escalated));
    assert!(WorkflowState::Investigating.can_transition_to(&WorkflowState::Synthesizing));
    assert!(WorkflowState::Synthesizing.can_transition_to(&WorkflowState::Resolved));
    assert!(WorkflowState::Synthesizing.can_transition_to(&WorkflowState::Escalated));
}

#[test]
fn test_invalid_state_transitions() {
    assert!(!WorkflowState::Created.can_transition_to(&WorkflowState::Resolved));
    assert!(!WorkflowState::Created.can_transition_to(&WorkflowState::Investigating));
    assert!(!WorkflowState::Resolved.can_transition_to(&WorkflowState::Created));
    assert!(!WorkflowState::Failed.can_transition_to(&WorkflowState::Triaging));
}

#[test]
fn test_terminal_states() {
    assert!(WorkflowState::Resolved.is_terminal());
    assert!(WorkflowState::Escalated.is_terminal());
    assert!(WorkflowState::Failed.is_terminal());
    assert!(!WorkflowState::Created.is_terminal());
    assert!(!WorkflowState::Investigating.is_terminal());
}
