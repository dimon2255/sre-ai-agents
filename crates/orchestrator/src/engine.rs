use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc;
use tracing::{error, info, instrument, warn};
use uuid::Uuid;

use sre_common::config::PlatformConfig;
use sre_common::errors::OrchestratorError;
use sre_common::findings::InvestigationReport;
use sre_common::messages::*;
use sre_store::models::WorkflowState;
use sre_store::traits::{EventLog, WorkflowStore};

use crate::aggregator::FindingAggregator;
use crate::dedup::AlertDedupCache;
use crate::router::AgentRouter;
use crate::timeout;
use crate::workflow::InvestigationWorkflow;

/// Minimum dedup cache capacity, regardless of channel_capacity setting.
const MIN_DEDUP_CACHE_CAPACITY: usize = 64;

/// Multiplier applied to `webhook.channel_capacity` to size the dedup cache.
/// Default channel_capacity=64 yields a 640-entry cache.
const DEDUP_CACHE_CAPACITY_MULTIPLIER: usize = 10;

/// Distinct finding-category names (Debug form, e.g. `"ErrorSpike"`),
/// order-preserving. Used to key knowledge-store records and confidence stats.
fn finding_category_names(findings: &[sre_common::findings::Finding]) -> Vec<String> {
    let mut names = Vec::new();
    for f in findings {
        let name = format!("{:?}", f.category);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    names
}

/// The central orchestrator that coordinates investigations (ADR-003, ADR-006).
///
/// Responsibilities:
/// - Receive investigation triggers (alerts, manual requests)
/// - Route tasks to appropriate agents
/// - Manage workflow state transitions
/// - Aggregate findings and generate reports
/// - Handle timeouts and partial results
///
/// Each investigation creates its own reply channel (ADR-006), eliminating
/// the shared-inbox race condition. Agents reply through the channel bundled
/// with the task message, so responses are always scoped to the correct
/// investigation.
pub struct Orchestrator {
    config: PlatformConfig,
    router: AgentRouter,
    /// Store for persisting workflow state and audit trail
    store: Arc<dyn WorkflowStore>,
    event_log: Arc<dyn EventLog>,
    /// In-memory dedup cache for at-least-once ingest paths (ADR-013).
    /// Mutex is fine: critical sections are O(1) and never cross an await.
    dedup_cache: Arc<Mutex<AlertDedupCache>>,
    /// Optional gated-remediation pipeline (Phase 2c, ADR-009). `None` unless
    /// remediation is enabled; set via [`Orchestrator::with_remediation`].
    remediation: Option<crate::remediation::RemediationPipeline>,
    /// Optional LLM reasoning engine (Phase 3, ADR-015). `None` unless enabled;
    /// set via [`Orchestrator::with_reasoning`].
    reasoning: Option<Arc<dyn sre_intelligence::ReasoningEngine>>,
    /// Optional knowledge store for RAG grounding (Phase 3, ADR-016). `None`
    /// unless enabled; set via [`Orchestrator::with_knowledge`]. Read before
    /// reasoning to retrieve similar past incidents.
    knowledge: Option<Arc<dyn sre_intelligence::KnowledgeStore>>,
    /// Optional learning loop (Phase 3, ADR-016). `None` unless enabled; set via
    /// [`Orchestrator::with_learning`]. Embeds each completed incident.
    learning: Option<Arc<sre_intelligence::LearningLoop>>,
}

impl Orchestrator {
    pub fn new(
        config: PlatformConfig,
        router: AgentRouter,
        store: Arc<dyn WorkflowStore>,
        event_log: Arc<dyn EventLog>,
    ) -> Self {
        let dedup_capacity = (config.webhook.channel_capacity * DEDUP_CACHE_CAPACITY_MULTIPLIER)
            .max(MIN_DEDUP_CACHE_CAPACITY);
        Self {
            config,
            router,
            store,
            event_log,
            dedup_cache: Arc::new(Mutex::new(AlertDedupCache::new(dedup_capacity))),
            remediation: None,
            reasoning: None,
            knowledge: None,
            learning: None,
        }
    }

    /// Construct an Orchestrator with an explicit dedup cache. Test-only —
    /// production callers should use `new()` which sizes the cache from
    /// `webhook.channel_capacity`.
    #[doc(hidden)]
    pub fn new_with_dedup(
        config: PlatformConfig,
        router: AgentRouter,
        store: Arc<dyn WorkflowStore>,
        event_log: Arc<dyn EventLog>,
        dedup_cache: Arc<Mutex<AlertDedupCache>>,
    ) -> Self {
        Self {
            config,
            router,
            store,
            event_log,
            dedup_cache,
            remediation: None,
            reasoning: None,
            knowledge: None,
            learning: None,
        }
    }

    /// Attach a gated-remediation pipeline (Phase 2c, ADR-009). Builder style so
    /// the existing `new` / `new_with_dedup` call sites are unaffected.
    pub fn with_remediation(mut self, pipeline: crate::remediation::RemediationPipeline) -> Self {
        self.remediation = Some(pipeline);
        self
    }

    /// Attach an LLM reasoning engine (Phase 3, ADR-015). Builder style.
    pub fn with_reasoning(mut self, engine: Arc<dyn sre_intelligence::ReasoningEngine>) -> Self {
        self.reasoning = Some(engine);
        self
    }

    /// Attach a knowledge store for RAG grounding (Phase 3, ADR-016). Builder
    /// style. Retrieved past incidents ground the reasoning engine's RCA.
    pub fn with_knowledge(mut self, store: Arc<dyn sre_intelligence::KnowledgeStore>) -> Self {
        self.knowledge = Some(store);
        self
    }

    /// Attach a learning loop (Phase 3, ADR-016). Builder style. Each completed
    /// investigation is embedded back into the knowledge store.
    pub fn with_learning(mut self, learning: Arc<sre_intelligence::LearningLoop>) -> Self {
        self.learning = Some(learning);
        self
    }

    /// Retrieve similar past incidents for RAG grounding (Phase 3, ADR-016).
    /// Best-effort: returns empty when no knowledge store is configured or on any
    /// retrieval error, so reasoning proceeds ungrounded rather than failing.
    async fn retrieve_related(
        &self,
        report: &InvestigationReport,
        alert: &AlertInfo,
    ) -> Vec<String> {
        let Some(store) = self.knowledge.as_ref() else {
            return Vec::new();
        };
        let query = sre_intelligence::IncidentQuery {
            affected_resource: report.affected_resource.clone(),
            trigger: report.trigger.clone(),
            severity: alert.severity.to_string(),
            summary: report.summary.clone(),
            finding_categories: finding_category_names(&report.findings),
        };
        match store.search(&query, self.config.knowledge.top_k).await {
            Ok(hits) => {
                if !hits.is_empty() {
                    info!(
                        count = hits.len(),
                        engine = store.name(),
                        "Retrieved related past incidents for RAG grounding"
                    );
                }
                hits.iter().map(|h| h.grounding_line()).collect()
            }
            Err(e) => {
                warn!(error = %e, "Knowledge retrieval failed — proceeding without grounding");
                Vec::new()
            }
        }
    }

    /// Embed the completed incident into the knowledge store via the learning
    /// loop (Phase 3, ADR-016). Best-effort, opt-in. This is the read-only
    /// investigation path, so the outcome classifies as `Inconclusive` (no
    /// remediation was executed here); the remediation pipeline records real
    /// success/failure outcomes separately.
    async fn record_incident(
        &self,
        report: &InvestigationReport,
        alert: &AlertInfo,
        rca_root_cause: Option<String>,
    ) {
        let Some(learning) = self.learning.as_ref() else {
            return;
        };
        let record = sre_intelligence::IncidentRecord {
            investigation_id: report.investigation_id.clone(),
            affected_resource: report.affected_resource.clone(),
            trigger: report.trigger.clone(),
            severity: alert.severity.to_string(),
            root_cause: rca_root_cause.unwrap_or_else(|| report.summary.clone()),
            summary: report.summary.clone(),
            finding_categories: finding_category_names(&report.findings),
            remediation: None,
            outcome: None,
            recorded_at: Some(report.completed_at.to_rfc3339()),
        };
        if let Err(e) = learning
            .record_outcome(record, sre_intelligence::OutcomeSignals::default())
            .await
        {
            warn!(error = %e, "Failed to embed incident into knowledge store");
        }
    }

    /// Run the gated-remediation pipeline for a completed investigation
    /// (Phase 2c, ADR-009). No-op when remediation is not configured. Returns
    /// the number of proposals processed. Each lifecycle transition is appended
    /// to the event log as `AgentPayload::RemediationLifecycle` (insert-only
    /// audit trail, ADR-009 §7).
    pub async fn remediate(&self, report: &InvestigationReport) -> usize {
        let Some(pipeline) = self.remediation.as_ref() else {
            return 0;
        };
        let Some(ctx) = crate::remediation::build_context(report) else {
            warn!(investigation_id = %report.investigation_id, "could not build remediation context");
            return 0;
        };
        let proposals = pipeline.agent.propose(&ctx);
        if proposals.is_empty() {
            return 0;
        }
        let now = Utc::now();
        for proposal in &proposals {
            let events = pipeline.executor.process(proposal, now).await;
            for event in &events {
                let msg = AgentMessage::new(
                    ctx.investigation_id,
                    AgentId("sre-gates".to_string()),
                    MessageTarget::Orchestrator,
                    AgentPayload::RemediationLifecycle(event.clone()),
                    1.0,
                );
                if let Err(e) = self.event_log.append(&msg).await {
                    warn!(error = %e, "failed to append remediation audit event");
                }
            }
        }
        info!(
            investigation_id = %report.investigation_id,
            proposals = proposals.len(),
            "Remediation pipeline processed proposals"
        );
        proposals.len()
    }

    /// Append an `IngestDeduped` audit message to the event log so the
    /// durable trail reflects skipped duplicates (ADR-013). Uses a fresh
    /// correlation_id because the dedup decision happens before — or
    /// instead of — a workflow being created. Audit failures are logged
    /// but never propagated; this is best-effort instrumentation.
    async fn audit_dedup(&self, alert: &AlertInfo, source: DedupSource) {
        let msg = AgentMessage::new(
            Uuid::new_v4(),
            AgentId("orchestrator".to_string()),
            MessageTarget::Orchestrator,
            AgentPayload::IngestDeduped(IngestDedupedNotice {
                alert_id: alert.alert_id.clone(),
                affected_resource: alert.affected_resource.clone(),
                source,
            }),
            1.0,
        );
        if let Err(e) = self.event_log.append(&msg).await {
            warn!(error = %e, alert_id = %alert.alert_id, "Failed to append dedup audit event");
        }
    }

    /// Build a minimal report representing a duplicate alert that was
    /// skipped without dispatching agents. Kept as a method so the
    /// dispatch loop's existing logging contract (`Ok(report)` with
    /// `duration_ms`, `findings.len()`, `overall_confidence`) is honored.
    fn duplicate_skip_report(alert: &AlertInfo) -> InvestigationReport {
        let now = Utc::now();
        InvestigationReport {
            investigation_id: format!("dup-{}", alert.alert_id),
            started_at: now,
            completed_at: now,
            duration_ms: 0,
            trigger: alert.alert_id.clone(),
            affected_resource: alert.affected_resource.clone(),
            overall_confidence: 0.0,
            findings: Vec::new(),
            summary: format!(
                "Duplicate alert {} — already seen recently, investigation skipped (ADR-013).",
                alert.alert_id
            ),
            agents_invoked: Vec::new(),
            escalated: false,
            escalation_reason: None,
        }
    }

    /// Run a full investigation for a given alert.
    ///
    /// This is the main investigation loop:
    /// 1. Create workflow + per-investigation reply channel
    /// 2. Send to TriageAgent
    /// 3. Fan out to investigation agents based on triage result
    /// 4. Collect results with timeout (all on the scoped channel)
    /// 5. Aggregate findings and generate report
    #[instrument(skip(self), fields(
        alert_id = %alert.alert_id,
        resource = %alert.affected_resource,
        severity = %alert.severity,
        signal_type = %alert.signal_type,
        workflow_id,
        investigation.outcome,
        investigation.findings_count,
        investigation.confidence,
        investigation.duration_ms,
    ))]
    pub async fn investigate(
        &self,
        alert: AlertInfo,
    ) -> Result<InvestigationReport, OrchestratorError> {
        // At-least-once dedup, layer 1 (ADR-013). NATS JetStream and webhook
        // retries can both surface the same alert_id; the in-memory cache
        // keeps intra-replica duplicate dispatch off the hot path without DB
        // hits. Mutex poison is recovered from rather than propagated — the
        // cache is a performance optimization, not a correctness primitive.
        //
        // The lookup is scoped tightly so the `MutexGuard` is dropped before
        // any `.await` below — `MutexGuard` is `!Send` and holding it across
        // an await makes the surrounding future `!Send`, which breaks
        // `tokio::spawn` on the dispatch side.
        let is_duplicate = {
            let mut cache = self
                .dedup_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            !cache.check_and_insert(&alert.alert_id)
        };
        if is_duplicate {
            warn!(
                alert_id = %alert.alert_id,
                resource = %alert.affected_resource,
                "Duplicate alert skipped — in-memory LRU hit"
            );
            self.audit_dedup(&alert, DedupSource::InMemoryLru).await;
            return Ok(Self::duplicate_skip_report(&alert));
        }

        let mut workflow = InvestigationWorkflow::new(&alert);
        let correlation_id = *workflow.id();
        tracing::Span::current().record("workflow_id", tracing::field::display(&correlation_id));
        let agent_timeout = Duration::from_secs(self.config.orchestrator.agent_timeout_secs);

        // Per-investigation reply channel (ADR-006)
        let (reply_tx, mut reply_rx) = mpsc::channel::<AgentMessage>(64);

        info!(
            workflow_id = %correlation_id,
            severity = %alert.severity,
            "Starting investigation"
        );

        // At-least-once dedup, layer 2 (ADR-013): the DB UNIQUE constraint
        // on workflows.alert_id is the authoritative cross-replica gate.
        // `try_create_workflow` returns false when an existing workflow
        // already owns this alert_id (another replica claimed it, or the
        // same alert arrived via both webhook and NATS). Errors are logged
        // but do not stop the investigation — best-effort dedup, never a
        // hard stop on the alert path.
        match self.store.try_create_workflow(&workflow.record).await {
            Ok(true) => {}
            Ok(false) => {
                warn!(
                    alert_id = %alert.alert_id,
                    resource = %alert.affected_resource,
                    "Duplicate alert skipped — DB unique constraint"
                );
                self.audit_dedup(&alert, DedupSource::DbUniqueConstraint)
                    .await;
                return Ok(Self::duplicate_skip_report(&alert));
            }
            Err(e) => {
                error!(error = %e, "Failed to persist workflow — proceeding without DB dedup gate");
            }
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
            .route_to_type(&AgentType::Triage, triage_msg, reply_tx.clone())
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

        // Wait for triage response on the scoped channel
        let triage_response =
            timeout::recv_with_timeout(&mut reply_rx, agent_timeout, "triage").await;

        let triage_result = match triage_response {
            Some(msg) => {
                let _ = self.event_log.append(&msg).await;
                match msg.payload {
                    AgentPayload::TriageResult(result) => result,
                    other => {
                        warn!(
                            "Unexpected payload from triage: {:?}",
                            std::mem::discriminant(&other)
                        );
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

        workflow.transition_to(WorkflowState::Investigating)?;
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
                AgentType::Trace => AnalysisTaskType::TraceCorrelation,
                AgentType::Infra => AnalysisTaskType::InfraChanges,
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

            if let Err(e) = self
                .router
                .route_to_type(agent_type, msg, reply_tx.clone())
                .await
            {
                warn!(agent_type = %agent_type, error = %e, "Failed to route to agent");
            } else {
                expected_responses += 1;
            }
        }

        // ── Step 3: Collect results on the scoped channel ──

        let responses = timeout::collect_with_timeout(
            &mut reply_rx,
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

        workflow.transition_to(WorkflowState::Synthesizing)?;
        if let Err(e) = self
            .store
            .update_state(&correlation_id, WorkflowState::Synthesizing)
            .await
        {
            error!(error = %e, "Failed to persist state");
        }

        // Determine final state before generating report so escalation is reflected
        let final_state = if FindingAggregator::should_escalate(workflow.findings()) {
            WorkflowState::Escalated
        } else {
            WorkflowState::Resolved
        };

        workflow.transition_to(final_state)?;
        let _ = self.store.update_state(&correlation_id, final_state).await;

        let mut report = workflow.generate_report();

        // Phase 3 (ADR-016): retrieve similar past incidents for RAG grounding.
        // Only worth doing when a reasoning engine will consume them; best-effort.
        let related_incidents = if self.reasoning.is_some() {
            self.retrieve_related(&report, &alert).await
        } else {
            Vec::new()
        };

        // Phase 3 (ADR-015): enrich the report with an LLM-synthesized RCA when a
        // reasoning engine is configured. Best-effort — a failure keeps the
        // rule-based summary and never fails the investigation.
        let mut rca_root_cause: Option<String> = None;
        if let Some(engine) = self.reasoning.as_ref() {
            let input = sre_intelligence::ReasoningInput {
                investigation_id: report.investigation_id.clone(),
                affected_resource: report.affected_resource.clone(),
                trigger: report.trigger.clone(),
                severity: alert.severity.to_string(),
                findings: report.findings.clone(),
                agent_summary: report.summary.clone(),
                overall_confidence: report.overall_confidence,
                related_incidents,
            };
            match engine.synthesize(&input).await {
                Ok(rca) => {
                    info!(
                        engine = engine.name(),
                        root_cause = %rca.root_cause,
                        confidence = rca.confidence,
                        "LLM RCA synthesized"
                    );
                    let actions = if rca.recommended_actions.is_empty() {
                        "escalate".to_string()
                    } else {
                        rca.recommended_actions.join("; ")
                    };
                    report.summary = format!(
                        "[RCA · {}] {}\nConfidence: {:.0}%. Recommended: {}.\n{}\n\n— Agent findings: {}",
                        engine.name(),
                        rca.root_cause,
                        rca.confidence * 100.0,
                        actions,
                        rca.narrative,
                        report.summary,
                    );
                    rca_root_cause = Some(rca.root_cause);
                }
                Err(e) => {
                    warn!(error = %e, "LLM RCA synthesis failed — keeping rule-based summary")
                }
            }
        }

        // Phase 3 (ADR-016): embed the completed incident into the knowledge
        // store via the learning loop, so future investigations retrieve it.
        self.record_incident(&report, &alert, rca_root_cause).await;

        let _ = self.store.store_report(&correlation_id, &report).await;

        // Record outcome fields on the root span for trace filtering
        let span = tracing::Span::current();
        span.record(
            "investigation.outcome",
            tracing::field::display(&final_state),
        );
        span.record("investigation.findings_count", report.findings.len());
        span.record("investigation.confidence", report.overall_confidence);
        span.record("investigation.duration_ms", report.duration_ms);

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
