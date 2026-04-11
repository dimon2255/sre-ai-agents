use sre_common::findings::*;
use sre_common::messages::*;

/// Verify that AgentMessage survives a JSON roundtrip.
/// This validates NATS readiness — all messages must serialize/deserialize
/// correctly before we move to cross-process communication in Phase 2.
fn roundtrip<T: serde::Serialize + serde::de::DeserializeOwned + std::fmt::Debug>(value: &T) -> T {
    let json = serde_json::to_string(value).expect("serialize failed");
    serde_json::from_str(&json).expect("deserialize failed")
}

fn sample_alert() -> AlertInfo {
    AlertInfo {
        alert_id: "test-alert-001".to_string(),
        alert_rule: "High Error Rate".to_string(),
        severity: AlertSeverity::Sev1,
        signal_type: "Metric".to_string(),
        monitor_condition: "Fired".to_string(),
        affected_resource: "/subscriptions/abc/resourceGroups/rg/providers/Microsoft.Web/sites/api"
            .to_string(),
        fired_at: chrono::Utc::now(),
        description: Some("Error rate exceeded threshold".to_string()),
        raw_context: Some(serde_json::json!({"metricValue": 42.0})),
    }
}

fn sample_finding() -> Finding {
    Finding {
        category: FindingCategory::ErrorSpike,
        severity: FindingSeverity::High,
        title: "HTTP 500 spike detected".to_string(),
        description: "Error rate jumped from 0.5% to 12% over 5 minutes".to_string(),
        confidence: 0.85,
        observed_at: Some(chrono::Utc::now()),
        source_agent: "logs-agent".to_string(),
        evidence: vec![Evidence {
            evidence_type: EvidenceType::Metric,
            label: "Error count".to_string(),
            data: serde_json::json!({"count": 1500, "rate_pct": 12.0}),
        }],
        suggestions: vec!["Check recent deployments".to_string()],
    }
}

// ─── AlertInfo roundtrip ───

#[test]
fn test_alert_info_roundtrip() {
    let alert = sample_alert();
    let rt = roundtrip(&alert);
    assert_eq!(rt.alert_id, alert.alert_id);
    assert_eq!(rt.severity, alert.severity);
    assert_eq!(rt.affected_resource, alert.affected_resource);
    assert_eq!(rt.description, alert.description);
}

// ─── Finding roundtrip ───

#[test]
fn test_finding_roundtrip() {
    let finding = sample_finding();
    let rt = roundtrip(&finding);
    assert_eq!(rt.title, finding.title);
    assert_eq!(rt.severity, finding.severity);
    assert_eq!(rt.category, finding.category);
    assert!((rt.confidence - finding.confidence).abs() < f64::EPSILON);
}

// ─── AgentMessage + all payload variants ───

#[test]
fn test_agent_message_investigation_request_roundtrip() {
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("triage-agent".to_string())),
        AgentPayload::InvestigationRequest(InvestigationContext {
            alert: sample_alert(),
            additional_context: Some("triggered by oncall".to_string()),
        }),
        1.0,
    );
    let rt = roundtrip(&msg);
    assert_eq!(rt.correlation_id, msg.correlation_id);
    assert_eq!(rt.source_agent, msg.source_agent);
    assert!((rt.confidence - msg.confidence).abs() < f64::EPSILON);
}

#[test]
fn test_agent_message_triage_result_roundtrip() {
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("triage-agent".to_string()),
        MessageTarget::Orchestrator,
        AgentPayload::TriageResult(TriageResult {
            severity: AlertSeverity::Sev1,
            affected_service: "payments-api".to_string(),
            signal_summary: "Metric alert fired".to_string(),
            recommended_agents: vec![AgentType::Logs, AgentType::Metrics],
            investigation_params: InvestigationParams {
                resource_name: "payments-api".to_string(),
                resource_group: Some("prod-rg".to_string()),
                timespan_minutes: 30,
                operation_ids: vec![],
            },
        }),
        0.95,
    );
    let rt = roundtrip(&msg);
    if let AgentPayload::TriageResult(ref result) = rt.payload {
        assert_eq!(result.affected_service, "payments-api");
        assert_eq!(result.recommended_agents.len(), 2);
    } else {
        panic!("Expected TriageResult payload");
    }
}

#[test]
fn test_agent_message_analysis_result_roundtrip() {
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("logs-agent".to_string()),
        MessageTarget::Orchestrator,
        AgentPayload::AnalysisResult(AnalysisResult {
            findings: vec![sample_finding()],
            summary: "Found 1 error spike".to_string(),
            queries_executed: vec![QueryRecord {
                template_name: "error_spike".to_string(),
                parameters: serde_json::json!({"resource": "api", "timespan": 30}),
                rows_returned: 42,
                duration_ms: 150,
            }],
        }),
        0.85,
    );
    let rt = roundtrip(&msg);
    if let AgentPayload::AnalysisResult(ref result) = rt.payload {
        assert_eq!(result.findings.len(), 1);
        assert_eq!(result.queries_executed.len(), 1);
        assert_eq!(result.queries_executed[0].rows_returned, 42);
    } else {
        panic!("Expected AnalysisResult payload");
    }
}

#[test]
fn test_agent_message_escalation_roundtrip() {
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Broadcast,
        AgentPayload::EscalationNotice(EscalationNotice {
            reason: EscalationReason::LowConfidence,
            details: "Confidence below threshold".to_string(),
            partial_findings: vec![sample_finding()],
        }),
        0.3,
    );
    let rt = roundtrip(&msg);
    if let AgentPayload::EscalationNotice(ref notice) = rt.payload {
        assert_eq!(notice.partial_findings.len(), 1);
    } else {
        panic!("Expected EscalationNotice payload");
    }
}

#[test]
fn test_agent_message_health_check_roundtrip() {
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("triage-agent".to_string())),
        AgentPayload::HealthCheck,
        1.0,
    );
    let rt = roundtrip(&msg);
    assert!(matches!(rt.payload, AgentPayload::HealthCheck));
}

#[test]
fn test_agent_message_health_status_roundtrip() {
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("triage-agent".to_string()),
        MessageTarget::Orchestrator,
        AgentPayload::HealthStatus(AgentHealth {
            agent_id: AgentId("triage-agent".to_string()),
            status: AgentStatus::Ready,
            active_tasks: 0,
            uptime_secs: 3600,
        }),
        1.0,
    );
    let rt = roundtrip(&msg);
    if let AgentPayload::HealthStatus(ref health) = rt.payload {
        assert_eq!(health.status, AgentStatus::Ready);
        assert_eq!(health.uptime_secs, 3600);
    } else {
        panic!("Expected HealthStatus payload");
    }
}

// ─── InvestigationReport roundtrip ───

#[test]
fn test_investigation_report_roundtrip() {
    let report = InvestigationReport {
        investigation_id: uuid::Uuid::new_v4().to_string(),
        started_at: chrono::Utc::now(),
        completed_at: chrono::Utc::now(),
        duration_ms: 1500,
        trigger: "High Error Rate alert".to_string(),
        affected_resource: "payments-api".to_string(),
        overall_confidence: 0.75,
        findings: vec![sample_finding()],
        summary: "Found error spike in payments-api".to_string(),
        agents_invoked: vec![
            "triage-agent".to_string(),
            "logs-agent".to_string(),
            "metrics-agent".to_string(),
        ],
        escalated: false,
        escalation_reason: None,
    };
    let rt = roundtrip(&report);
    assert_eq!(rt.investigation_id, report.investigation_id);
    assert_eq!(rt.findings.len(), 1);
    assert!((rt.overall_confidence - 0.75).abs() < f64::EPSILON);
}

// ─── InboundMessage serialization boundary (Phase 2 NATS spike) ───

#[test]
fn test_inbound_message_contains_non_serializable_sender() {
    // This test documents the Phase 2 design challenge:
    // InboundMessage wraps AgentMessage + mpsc::Sender, which cannot
    // be serialized. For NATS, the Sender must be replaced with a
    // reply subject string. This test verifies the inner AgentMessage
    // IS serializable, confirming the boundary is clean.
    let (tx, _rx) = tokio::sync::mpsc::channel::<AgentMessage>(1);
    let msg = AgentMessage::new(
        uuid::Uuid::new_v4(),
        AgentId("orchestrator".to_string()),
        MessageTarget::Agent(AgentId("triage-agent".to_string())),
        AgentPayload::HealthCheck,
        1.0,
    );

    // The AgentMessage inside InboundMessage serializes fine
    let json = serde_json::to_string(&msg).unwrap();
    let _: AgentMessage = serde_json::from_str(&json).unwrap();

    // InboundMessage itself cannot be serialized (mpsc::Sender).
    // For Phase 2 NATS migration, replace:
    //   pub reply_tx: mpsc::Sender<AgentMessage>
    // with:
    //   pub reply_subject: String
    // and let the NATS layer handle routing.
    let _inbound = InboundMessage {
        message: msg,
        reply_tx: tx,
    };
    // Compile-time proof: InboundMessage does NOT implement Serialize.
    // If someone adds #[derive(Serialize)] to InboundMessage, this test
    // still passes — but the build would fail on mpsc::Sender.
}
