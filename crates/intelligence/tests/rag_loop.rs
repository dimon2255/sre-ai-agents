//! End-to-end RAG loop (Docker-free): the learning loop embeds a past incident,
//! the knowledge store retrieves it for a similar new incident, and the mock
//! reasoning engine consumes it as grounding. This exercises the same
//! retrieve → reason → record cycle the orchestrator wires in `investigate()`,
//! without needing Azure, a network, or Postgres.

use std::sync::Arc;

use sre_intelligence::{
    HashEmbedder, IncidentQuery, IncidentRecord, KnowledgeStore, LearningLoop, MockKnowledgeStore,
    MockReasoningEngine, OutcomeSignals, ReasoningEngine, ReasoningInput,
};

#[tokio::test]
async fn rag_loop_grounds_reasoning_with_past_incident() {
    let store = Arc::new(MockKnowledgeStore::new(Arc::new(HashEmbedder::new(256))));
    let learning = LearningLoop::new(store.clone());

    // 1. A past incident is recorded via the learning loop with a success outcome.
    let past = IncidentRecord {
        investigation_id: "past-1".into(),
        affected_resource: "payments-api".into(),
        trigger: "High Error Rate".into(),
        severity: "Sev1".into(),
        root_cause: "deploy regression returned 500s".into(),
        summary: "payments-api 500 spike after deploy; rolled back".into(),
        finding_categories: vec!["ErrorSpike".into(), "DeploymentCorrelation".into()],
        remediation: Some("RevertRevision".into()),
        outcome: None,
        recorded_at: None,
    };
    let outcome = learning
        .record_outcome(
            past,
            OutcomeSignals {
                remediation_executed: true,
                alert_resolved_within_window: true,
                alert_refired_within_hour: false,
            },
        )
        .await
        .unwrap();
    assert_eq!(outcome.as_str(), "success");
    assert_eq!(store.len(), 1);
    assert_eq!(learning.success_rate("ErrorSpike"), 1.0);

    // 2. A new, similar incident retrieves the past one as grounding.
    let query = IncidentQuery {
        affected_resource: "payments-api".into(),
        trigger: "High Error Rate".into(),
        severity: "Sev1".into(),
        summary: "payments-api returning 500 errors after a deploy".into(),
        finding_categories: vec!["ErrorSpike".into()],
    };
    let hits = store.search(&query, 3).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].investigation_id, "past-1");
    assert!(hits[0].summary.contains("payments-api"));
    let grounding: Vec<String> = hits.iter().map(|h| h.grounding_line()).collect();

    // 3. The reasoning engine consumes the retrieved grounding.
    let engine = MockReasoningEngine::new();
    let out = engine
        .synthesize(&ReasoningInput {
            investigation_id: "new-1".into(),
            affected_resource: "payments-api".into(),
            trigger: "High Error Rate".into(),
            severity: "Sev1".into(),
            findings: vec![],
            agent_summary: "errors observed".into(),
            overall_confidence: 0.8,
            related_incidents: grounding,
        })
        .await
        .unwrap();

    assert!(
        out.narrative.contains("related past incident"),
        "narrative should reflect the retrieved grounding: {}",
        out.narrative
    );
}
