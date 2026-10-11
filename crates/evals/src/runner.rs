//! Replays one scenario through the production investigation pipeline.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use uuid::Uuid;

use sre_agents::{
    AgentHandle, InfraAgent, LogsAgent, MetricsAgent, RemediationAgent, SreAgent, TraceAgent,
    TriageAgent,
};
use sre_azure::auth::MockTokenProvider;
use sre_azure::query_client::AzureQueryClient;
use sre_common::config::PlatformConfig;
use sre_common::errors::StoreError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::{AgentMessage, AgentPayload, AlertInfo, MessageTarget};
use sre_common::remediation::{RemediationCategory, RemediationProposal};
use sre_orchestrator::{AgentRouter, Orchestrator};
use sre_store::models::{WorkflowRecord, WorkflowState};
use sre_store::{EventLog, WorkflowStore};

use crate::fixture::{FixtureServer, QueryHit};
use crate::scenario::Scenario;

/// Workspace and App Insights ids the agents query; any value works because
/// the fixture server answers every path.
const WORKSPACE_ID: &str = "golden-workspace";
const APP_INSIGHTS_ID: &str = "golden-app-insights";
const MAX_ROWS: u32 = 1000;
const AGENT_TIMEOUT_SECS: u64 = 10;

/// A remediation the pipeline would propose, named by the action it would run.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProposedAction {
    pub action: String,
    pub confidence: f64,
}

/// Everything a scenario run produced.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub report: InvestigationReport,
    /// Sorted most-confident first, as `RemediationAgent::propose` returns them.
    pub proposals: Vec<ProposedAction>,
    pub queries: Vec<QueryHit>,
}

/// Name a proposal by the action it would execute. A rollback is filed under
/// `RestartWorkload`, so the ARM call is what tells it apart from a restart.
pub fn action_name(proposal: &RemediationProposal) -> String {
    match proposal.category {
        RemediationCategory::RestartWorkload
            if proposal
                .arm_request
                .resource_id
                .ends_with("/revisions/previous/activate") =>
        {
            "rollback_deployment".to_string()
        }
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.as_str().map(String::from))
            .unwrap_or_else(|| format!("{other:?}")),
    }
}

fn register<A: SreAgent + 'static>(
    router: &mut AgentRouter,
    handles: &mut Vec<JoinHandle<()>>,
    agent: A,
) {
    let (tx, rx) = mpsc::channel(64);
    router.register(AgentHandle {
        id: agent.id().clone(),
        agent_type: agent.agent_type(),
        sender: tx,
        capabilities: agent.capabilities(),
    });
    handles.push(tokio::spawn(async move {
        let _ = agent.run(rx).await;
    }));
}

fn alert_for(scenario: &Scenario) -> AlertInfo {
    AlertInfo {
        // Neutral id: it reaches the LLM tier as the alert trigger, so it must
        // not hint at the scenario or that this is an evaluation.
        alert_id: Uuid::new_v4().to_string(),
        alert_rule: scenario.alert.rule.clone(),
        severity: scenario.alert.severity,
        signal_type: scenario.alert.signal_type.clone(),
        monitor_condition: "Fired".to_string(),
        affected_resource: scenario.alert.resource.clone(),
        fired_at: chrono::Utc::now(),
        description: scenario.alert.description.clone(),
        raw_context: scenario.alert.raw_context.clone(),
    }
}

/// Run one scenario end to end: fixture server, five agents, orchestrator,
/// then the remediation proposer over the finished report.
pub async fn run_scenario(scenario: &Scenario) -> anyhow::Result<RunOutcome> {
    let fixtures = FixtureServer::start(scenario.telemetry.clone()).await;
    let credential: Arc<dyn sre_azure::TokenProvider> =
        Arc::new(MockTokenProvider::new("golden-set-token"));
    let client = Arc::new(
        AzureQueryClient::new(credential)
            .with_log_analytics_base_url(fixtures.uri())
            .with_app_insights_base_url(fixtures.uri()),
    );
    let workspaces = vec![WORKSPACE_ID.to_string()];
    let apps = vec![APP_INSIGHTS_ID.to_string()];

    let mut router = AgentRouter::new();
    let mut handles = Vec::new();
    register(&mut router, &mut handles, TriageAgent::new());
    register(
        &mut router,
        &mut handles,
        LogsAgent::new(client.clone(), workspaces.clone(), MAX_ROWS),
    );
    register(
        &mut router,
        &mut handles,
        MetricsAgent::new(client.clone(), apps.clone()),
    );
    register(
        &mut router,
        &mut handles,
        TraceAgent::new(client.clone(), apps, MAX_ROWS),
    );
    register(
        &mut router,
        &mut handles,
        InfraAgent::new(client, workspaces, MAX_ROWS),
    );

    let mut config = PlatformConfig::default();
    config.orchestrator.agent_timeout_secs = AGENT_TIMEOUT_SECS;
    let store = Arc::new(MemStore::default());
    let orchestrator = Orchestrator::new(config, router, store.clone(), store.clone());

    let result = orchestrator.investigate(alert_for(scenario)).await;
    drop(orchestrator);
    for handle in &handles {
        handle.abort();
    }
    let report =
        result.map_err(|e| anyhow::anyhow!("{}: investigation failed: {e}", scenario.id))?;
    ensure_complete(&scenario.id, &report, &store)?;

    let proposals = sre_orchestrator::remediation::build_context(&report)
        .map(|ctx| {
            RemediationAgent::new()
                .propose(&ctx)
                .iter()
                .map(|p| ProposedAction {
                    action: action_name(p),
                    confidence: p.confidence,
                })
                .collect()
        })
        .unwrap_or_default();

    Ok(RunOutcome {
        report,
        proposals,
        queries: fixtures.hits(),
    })
}

/// Refuse to score an investigation the pipeline did not finish. The
/// orchestrator turns a triage timeout or a lost agent into a normal,
/// escalated report with no findings, which would otherwise score as a correct
/// "claimed nothing and paged a human". The audit log shows what was asked of
/// which agent and what came back.
fn ensure_complete(id: &str, report: &InvestigationReport, store: &MemStore) -> anyhow::Result<()> {
    let correlation_id = Uuid::parse_str(&report.investigation_id)
        .map_err(|_| anyhow::anyhow!("{id}: report has no investigation id (duplicate skip?)"))?;
    let events = store.events.lock().unwrap();
    let mine = || events.iter().filter(|m| m.correlation_id == correlation_id);

    if !mine().any(|m| matches!(m.payload, AgentPayload::TriageResult(_))) {
        anyhow::bail!("{id}: triage never replied; the pipeline did not run");
    }
    let tasked: BTreeSet<String> = mine()
        .filter(|m| matches!(m.payload, AgentPayload::AnalysisTask(_)))
        .filter_map(|m| match &m.target {
            MessageTarget::Agent(agent) => Some(agent.0.clone()),
            _ => None,
        })
        .collect();
    let replied: BTreeSet<String> = mine()
        .filter(|m| matches!(m.payload, AgentPayload::AnalysisResult(_)))
        .map(|m| m.source_agent.0.clone())
        .collect();
    let missing: Vec<&String> = tasked.difference(&replied).collect();
    if !missing.is_empty() {
        anyhow::bail!("{id}: no reply from {missing:?}; the pipeline did not finish");
    }
    Ok(())
}

/// Minimal in-memory implementation of both persistence traits. Persistence is
/// not under evaluation, and this keeps the eval free of Docker.
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
        let mut workflows = self.workflows.lock().unwrap();
        if workflows.values().any(|w| w.alert_id == workflow.alert_id) {
            return Ok(false);
        }
        workflows.insert(workflow.id, workflow.clone());
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
            w.report_json = serde_json::to_string(report).ok();
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

#[cfg(test)]
mod tests {
    use super::*;
    use sre_common::messages::{
        AgentId, AgentType, AlertSeverity, AnalysisTask, AnalysisTaskType, InvestigationParams,
        TriageResult,
    };

    fn params() -> InvestigationParams {
        InvestigationParams {
            resource_name: "svc".to_string(),
            resource_group: None,
            timespan_minutes: 30,
            operation_ids: vec![],
        }
    }

    fn report(id: Uuid) -> InvestigationReport {
        let now = chrono::Utc::now();
        InvestigationReport {
            investigation_id: id.to_string(),
            started_at: now,
            completed_at: now,
            duration_ms: 0,
            trigger: "t".to_string(),
            affected_resource: "svc".to_string(),
            overall_confidence: 0.0,
            findings: vec![],
            summary: String::new(),
            agents_invoked: vec![],
            escalated: true,
            escalation_reason: None,
        }
    }

    fn msg(id: Uuid, from: &str, to: MessageTarget, payload: AgentPayload) -> AgentMessage {
        AgentMessage::new(id, AgentId(from.to_string()), to, payload, 1.0)
    }

    #[test]
    fn run_without_triage_reply_is_rejected() {
        let id = Uuid::new_v4();
        let err = ensure_complete("GS-X", &report(id), &MemStore::default()).unwrap_err();
        assert!(err.to_string().contains("triage never replied"), "{err}");
    }

    #[test]
    fn run_with_a_silent_agent_is_rejected_and_a_full_run_passes() {
        let id = Uuid::new_v4();
        let store = MemStore::default();
        let triage = TriageResult {
            severity: AlertSeverity::Sev1,
            affected_service: "svc".to_string(),
            signal_summary: String::new(),
            recommended_agents: vec![AgentType::Logs],
            investigation_params: params(),
        };
        let task = AnalysisTask {
            task_type: AnalysisTaskType::ErrorAnalysis,
            params: params(),
        };
        {
            let mut events = store.events.lock().unwrap();
            events.push(msg(
                id,
                "triage-agent",
                MessageTarget::Orchestrator,
                AgentPayload::TriageResult(triage),
            ));
            events.push(msg(
                id,
                "orchestrator",
                MessageTarget::Agent(AgentId("logs-agent".to_string())),
                AgentPayload::AnalysisTask(task),
            ));
        }
        let err = ensure_complete("GS-X", &report(id), &store).unwrap_err();
        assert!(err.to_string().contains("logs-agent"), "{err}");

        store.events.lock().unwrap().push(msg(
            id,
            "logs-agent",
            MessageTarget::Orchestrator,
            AgentPayload::AnalysisResult(sre_common::messages::AnalysisResult {
                findings: vec![],
                summary: String::new(),
                queries_executed: vec![],
            }),
        ));
        ensure_complete("GS-X", &report(id), &store).unwrap();
    }
}
