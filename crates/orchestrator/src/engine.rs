use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{error, info, instrument, warn};

use sre_common::config::PlatformConfig;
use sre_common::errors::OrchestratorError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::*;
use sre_store::models::WorkflowState;
use sre_store::traits::{EventLog, WorkflowStore};

use crate::aggregator::FindingAggregator;
use crate::router::AgentRouter;
use crate::timeout;
use crate::workflow::InvestigationWorkflow;

/// The central orchestrator that coordinates investigations.
///
/// Responsibilities:
/// - Receive investigation triggers (alerts, manual requests)
/// - Route tasks to appropriate agents
/// - Manage workflow state transitions
/// - Aggregate findings and generate reports
/// - Handle timeouts and partial results
pub struct Orchestrator {
    config: PlatformConfig,
    router: AgentRouter,
    /// Orchestrator's inbox — all agents send responses here
    inbox: mpsc::Receiver<AgentMessage>,
    /// Store for persisting workflow state and audit trail
    store: Arc<dyn WorkflowStore>,
    event_log: Arc<dyn EventLog>,
}

impl Orchestrator {
    pub fn new(
        config: PlatformConfig,
        router: AgentRouter,
        inbox: mpsc::Receiver<AgentMessage>,
        store: Arc<dyn WorkflowStore>,
        event_log: Arc<dyn EventLog>,
    ) -> Self {
        Self {
            config,
            router,
            inbox,
            store,
            event_log,
        }
    }

    /// Run a full investigation for a given alert.
    ///
    /// This is the main investigation loop:
    /// 1. Create workflow
    /// 2. Send to TriageAgent
    /// 3. Fan out to investigation agents based on triage result
    /// 4. Collect results with timeout
    /// 5. Aggregate findings and generate report
    #[instrument(skip(self), fields(alert_id = %alert.alert_id, resource = %alert.affected_resource))]
    pub async fn investigate(
        &mut self,
        alert: AlertInfo,
    ) -> Result<InvestigationReport, OrchestratorError> {
        let mut workflow = InvestigationWorkflow::new(&alert);
        let correlation_id = *workflow.id();
        let agent_timeout =
            Duration::from_secs(self.config.orchestrator.agent_timeout_secs);

        info!(
            workflow_id = %correlation_id,
            severity = %alert.severity,
            "Starting investigation"
        );

        // Persist initial workflow state
        if let Err(e) = self.store.create_workflow(&workflow.record).await {
            error!(error = %e, "Failed to persist workflow");
        }

        // ── Step 1: Triage ──

        workflow
            .transition_to(WorkflowState::Triaging)
            .map_err(|e| {
                error!(error = %e, "Invalid state transition");
                e
            })?;

        if let Err(e) = self
            .store
            .update_state(&correlation_id, WorkflowState::Triaging)
            .await
        {
            error!(error = %e, "Failed to persist state");
        }

        let triage_msg = AgentMessage::new(
            correlation_id,
            AgentId("orchestrator".to_string()),
            MessageTarget::Agent(AgentId("triage-agent".to_string())),
            AgentPayload::InvestigationRequest(InvestigationContext {
                alert: alert.clone(),
                additional_context: None,
            }),
            1.0,
        );

        // Log the outbound message
        let _ = self.event_log.append(&triage_msg).await;

        if let Err(e) = self
            .router
            .route_to_type(&AgentType::Triage, triage_msg)
            .await
        {
            error!(error = %e, "Failed to route to TriageAgent");
            workflow.transition_to(WorkflowState::Failed)?;
            let _ = self
                .store
                .update_state(&correlation_id, WorkflowState::Failed)
                .await;
            return Ok(workflow.generate_report());
        }

        // Wait for triage response
        let triage_response = timeout::recv_with_timeout(
            &mut self.inbox,
            agent_timeout,
            "triage",
        )
        .await;

        let triage_result = match triage_response {
            Some(msg) => {
                let _ = self.event_log.append(&msg).await;
                match msg.payload {
                    AgentPayload::TriageResult(result) => result,
                    other => {
                        warn!("Unexpected payload from triage: {:?}", std::mem::discriminant(&other));
                        workflow.transition_to(WorkflowState::Failed)?;
                        return Ok(workflow.generate_report());
                    }
                }
            }
            None => {
                warn!("TriageAgent timed out");
                workflow.transition_to(WorkflowState::Failed)?;
                let _ = self
                    .store
                    .update_state(&correlation_id, WorkflowState::Failed)
                    .await;
                return Ok(workflow.generate_report());
            }
        };

        // ── Step 2: Fan out to investigation agents ──

        workflow
            .transition_to(WorkflowState::Investigating)?;
        if let Err(e) = self
            .store
            .update_state(&correlation_id, WorkflowState::Investigating)
            .await
        {
            error!(error = %e, "Failed to persist state");
        }

        let recommended_agents = triage_result.recommended_agents.clone();
        let investigation_params = triage_result.investigation_params.clone();
        workflow.record_triage(triage_result);

        let mut expected_responses = 0u32;

        for agent_type in &recommended_agents {
            if !self.router.has_agent_type(agent_type) {
                warn!(agent_type = %agent_type, "Agent type not available, skipping");
                continue;
            }

            let task_type = match agent_type {
                AgentType::Logs => AnalysisTaskType::ErrorAnalysis,
                AgentType::Metrics => AnalysisTaskType::PerformanceAnalysis,
                _ => continue,
            };

            let msg = AgentMessage::new(
                correlation_id,
                AgentId("orchestrator".to_string()),
                MessageTarget::Agent(AgentId(format!("{}-agent", agent_type))),
                AgentPayload::AnalysisTask(AnalysisTask {
                    task_type,
                    params: investigation_params.clone(),
                }),
                1.0,
            );

            let _ = self.event_log.append(&msg).await;

            if let Err(e) = self.router.route_to_type(agent_type, msg).await {
                warn!(agent_type = %agent_type, error = %e, "Failed to route to agent");
            } else {
                expected_responses += 1;
            }
        }

        // ── Step 3: Collect results ──

        let responses = timeout::collect_with_timeout(
            &mut self.inbox,
            expected_responses as usize,
            agent_timeout,
        )
        .await;

        info!(
            received = responses.len(),
            expected = expected_responses,
            "Agent responses collected"
        );

        for response in &responses {
            let _ = self.event_log.append(response).await;
            if let AgentPayload::AnalysisResult(ref result) = response.payload {
                workflow.record_analysis(&response.source_agent.0, result);
            }
        }

        // ── Step 4: Synthesize and report ──

        workflow
            .transition_to(WorkflowState::Synthesizing)?;
        if let Err(e) = self
            .store
            .update_state(&correlation_id, WorkflowState::Synthesizing)
            .await
        {
            error!(error = %e, "Failed to persist state");
        }

        let report = workflow.generate_report();

        // Determine final state
        let final_state = if FindingAggregator::should_escalate(&report.findings) {
            WorkflowState::Escalated
        } else {
            WorkflowState::Resolved
        };

        workflow.transition_to(final_state)?;
        let _ = self.store.update_state(&correlation_id, final_state).await;
        let _ = self.store.store_report(&correlation_id, &report).await;

        info!(
            workflow_id = %correlation_id,
            state = %final_state,
            findings_count = report.findings.len(),
            confidence = report.overall_confidence,
            duration_ms = report.duration_ms,
            "Investigation complete"
        );

        Ok(report)
    }
}
